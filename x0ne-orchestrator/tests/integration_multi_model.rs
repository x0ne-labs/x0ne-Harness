//! Integration test M — **multi-model** wire-format contracts.
//!
//! Vocabulary check (this matters):
//!
//! * **Multi-model** — X0NE speaks to several LLM *backends* (Anthropic,
//!   Ollama, OpenAI-compat). This file is about that: making sure the
//!   orchestrator-side `Message` struct serializes correctly into each
//!   backend's wire format, regardless of which backend the user picks.
//!
//! * **Multimodal** — a property of an individual *model* (text + images +
//!   audio + …). Vision happens to be the one multimodal feature X0NE
//!   currently exercises in the message layer (screenshots, image
//!   attachments), so most of the cross-backend assertions below pin down
//!   how an image-bearing `Message` lands in each backend's payload.
//!
//! In other words: this file proves that **multi-model parity holds even
//! when the message carries multimodal content.**
//!
//! Why it exists: X0NE already supports vision per backend (Anthropic
//! content blocks, Ollama `images` array, OpenAI `image_url` data-URLs)
//! but had **no end-to-end test** locking the wire format down. A future
//! refactor of `api.rs` could silently break llava / Claude vision /
//! GPT-4o without anyone noticing until a student loads the agent with
//! `--screenshot`.
//!
//! These tests are 100% offline: they exercise the public converters
//! `message_to_{anthropic,ollama,openai}_value`, plus a `Message` history round-trip (serde) for sessions persisted to
//! disk that contain image attachments.
//!
//! Each `#[test]` documents the contract it pins. If you change a wire
//! format on purpose, update the test in the same commit so reviewers can
//! see the format diff.

use x0ne_orchestrator::api::{
    message_to_anthropic_value, message_to_ollama_value, message_to_openai_value,
};
use x0ne_orchestrator::conversation::{Conversation, ImageAttachment, Message, Role};

// ── M.1 — Tiny helpers shared across tests ──────────────────────────────────

fn png(data: &str) -> ImageAttachment {
    ImageAttachment {
        media_type: "image/png".into(),
        data: data.into(),
    }
}

fn jpeg(data: &str) -> ImageAttachment {
    ImageAttachment {
        media_type: "image/jpeg".into(),
        data: data.into(),
    }
}

// ── M.2 — Anthropic vision shape ────────────────────────────────────────────

#[test]
fn anthropic_vision_shape_matches_official_schema() {
    // Contract: Anthropic vision messages are
    //   { role, content: [ {type:"text", text}, {type:"image", source:{type:"base64", media_type, data}}, ... ] }
    // (See https://docs.anthropic.com/.../vision)
    let m = Message {
        role: Role::User,
        content: "What's in this picture?".into(),
        images: vec![png("BASE64_PNG_BYTES")],
    };
    let v = message_to_anthropic_value(&m);

    assert_eq!(v["role"], "user");
    let blocks = v["content"].as_array().expect("vision => array of blocks");
    assert_eq!(blocks.len(), 2, "1 text + 1 image");

    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "What's in this picture?");

    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "BASE64_PNG_BYTES");
}

#[test]
fn anthropic_text_only_uses_string_content_not_blocks() {
    // Contract: text-only messages MUST keep the cheap string form.
    // Switching to blocks unconditionally inflates payload size and
    // disables prompt-caching keying on identical strings.
    let m = Message {
        role: Role::Assistant,
        content: "Sure, I can help.".into(),
        images: vec![],
    };
    let v = message_to_anthropic_value(&m);
    assert!(v["content"].is_string(), "got: {}", v["content"]);
    assert_eq!(v["content"], "Sure, I can help.");
}

// ── M.3 — Ollama vision shape ───────────────────────────────────────────────

#[test]
fn ollama_vision_shape_matches_llava_contract() {
    // Contract: Ollama (llava / bakllava / moondream / minicpm-v) accepts
    //   { role, content: "...", images: ["<base64_1>", "<base64_2>", ...] }
    // No MIME, no data-URL prefix, just raw base64 strings.
    let m = Message {
        role: Role::User,
        content: "[Screenshot captured]".into(),
        images: vec![png("FRAME_A"), jpeg("FRAME_B")],
    };
    let v = message_to_ollama_value(&m);

    assert_eq!(v["role"], "user");
    assert_eq!(v["content"], "[Screenshot captured]");
    let imgs = v["images"]
        .as_array()
        .expect("Ollama needs flat images array");
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs[0], "FRAME_A");
    assert_eq!(imgs[1], "FRAME_B");
    // Defensive: must NOT contain a data-URL — that's OpenAI's quirk.
    for s in imgs {
        let raw = s.as_str().unwrap();
        assert!(
            !raw.starts_with("data:"),
            "Ollama must receive raw base64, got data-URL"
        );
    }
}

#[test]
fn ollama_text_only_omits_images_field() {
    // Contract: when there are no images, the `images` key must be absent
    // (not `"images": []`). Some Ollama versions reject empty arrays.
    let m = Message {
        role: Role::User,
        content: "hello".into(),
        images: vec![],
    };
    let v = message_to_ollama_value(&m);
    assert!(
        v.get("images").is_none(),
        "got unexpected images field: {}",
        v
    );
    assert_eq!(v["content"], "hello");
}

// ── M.4 — OpenAI / vLLM / LiteLLM vision shape ──────────────────────────────

#[test]
fn openai_vision_shape_uses_data_url_image_blocks() {
    // Contract: OpenAI vision (GPT-4V / GPT-4o, mirrored by vLLM and
    // LiteLLM gateways) requires
    //   content: [ {type:"text", text}, {type:"image_url", image_url:{url:"data:<mime>;base64,<data>"}} ]
    let m = Message {
        role: Role::User,
        content: "describe the UI".into(),
        images: vec![png("UI_PNG"), jpeg("UI_JPG")],
    };
    let v = message_to_openai_value(&m);

    assert_eq!(v["role"], "user");
    let blocks = v["content"].as_array().expect("OpenAI vision => blocks");
    assert_eq!(blocks.len(), 3);

    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "describe the UI");

    assert_eq!(blocks[1]["type"], "image_url");
    assert_eq!(
        blocks[1]["image_url"]["url"],
        "data:image/png;base64,UI_PNG"
    );
    assert_eq!(
        blocks[2]["image_url"]["url"],
        "data:image/jpeg;base64,UI_JPG"
    );
}

#[test]
fn openai_text_only_keeps_string_content() {
    let m = Message {
        role: Role::User,
        content: "ping".into(),
        images: vec![],
    };
    let v = message_to_openai_value(&m);
    assert!(v["content"].is_string());
    assert_eq!(v["content"], "ping");
}

// ── M.5 — Cross-backend consistency ─────────────────────────────────────────

#[test]
fn empty_images_is_text_only_path_in_all_backends() {
    // Contract: the dispatcher logic in api.rs hinges on `images.is_empty()`.
    // All three backends MUST take the cheap text-only branch when no
    // images are attached, regardless of whether the message field exists
    // (it's `#[serde(default)]`).
    let m = Message {
        role: Role::User,
        content: "hi".into(),
        images: vec![],
    };

    let a = message_to_anthropic_value(&m);
    let o = message_to_ollama_value(&m);
    let p = message_to_openai_value(&m);

    assert!(
        a["content"].is_string(),
        "anthropic took blocks branch unexpectedly"
    );
    assert!(o.get("images").is_none(), "ollama emitted empty images key");
    assert!(
        p["content"].is_string(),
        "openai took blocks branch unexpectedly"
    );
}

#[test]
fn mixed_mime_types_propagate_per_image() {
    // Contract: each image's media_type travels independently. A bug
    // hard-coding "image/png" for all attachments would show here.
    let m = Message {
        role: Role::User,
        content: "compare".into(),
        images: vec![
            png("ALPHA"),
            jpeg("BETA"),
            ImageAttachment {
                media_type: "image/webp".into(),
                data: "GAMMA".into(),
            },
        ],
    };

    // Anthropic: media_type lives in source.media_type
    let a = message_to_anthropic_value(&m);
    let ablocks = a["content"].as_array().unwrap();
    assert_eq!(ablocks[1]["source"]["media_type"], "image/png");
    assert_eq!(ablocks[2]["source"]["media_type"], "image/jpeg");
    assert_eq!(ablocks[3]["source"]["media_type"], "image/webp");

    // OpenAI: media_type is encoded into the data-URL prefix
    let p = message_to_openai_value(&m);
    let pblocks = p["content"].as_array().unwrap();
    assert!(pblocks[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(pblocks[2]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/jpeg;base64,"));
    assert!(pblocks[3]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/webp;base64,"));

    // Ollama: bytes only, no MIME — that's the contract, verify it
    let o = message_to_ollama_value(&m);
    let oimgs = o["images"].as_array().unwrap();
    assert_eq!(oimgs.len(), 3);
    for img in oimgs {
        let s = img.as_str().unwrap();
        assert!(
            !s.contains("image/"),
            "Ollama must not leak MIME into image data"
        );
    }
}

#[test]
fn text_payload_is_not_duplicated_into_image_blocks() {
    // Regression guard: a previous bug emitted the `content` field both as
    // a string AND as a "text" block when images were present. That made
    // Claude render the question twice. Verify the text appears EXACTLY
    // once in the serialized payload.
    let m = Message {
        role: Role::User,
        content: "UNIQUE_QUESTION_MARKER".into(),
        images: vec![png("X")],
    };

    for (label, v) in [
        ("anthropic", message_to_anthropic_value(&m)),
        ("openai", message_to_openai_value(&m)),
        ("ollama", message_to_ollama_value(&m)),
    ] {
        let serialized = serde_json::to_string(&v).unwrap();
        let count = serialized.matches("UNIQUE_QUESTION_MARKER").count();
        assert_eq!(count, 1, "{}: text duplicated → {}", label, serialized);
    }
}

// ── M.6 — Conversation persistence with images ──────────────────────────────

#[test]
fn message_history_roundtrip_preserves_images_through_serde() {
    // Contract: sessions are persisted to disk as JSON arrays of Messages
    // (the `messages()` slice). Image attachments must survive a save→load
    // cycle so resuming a vision session works.
    let mut conv = Conversation::new(100_000);
    conv.add_user("describe");
    conv.add_user_with_image("[Screenshot]", vec![png("BEFORE_SAVE_DATA")]);
    conv.add_assistant("I see a terminal.");

    let serialized = serde_json::to_string(conv.messages()).expect("messages must serialize");
    assert!(
        serialized.contains("BEFORE_SAVE_DATA"),
        "image data missing from serialized session"
    );

    let restored: Vec<Message> =
        serde_json::from_str(&serialized).expect("messages must deserialize");
    assert_eq!(restored.len(), 3);
    assert_eq!(
        restored[1].images.len(),
        1,
        "image attachment lost on reload"
    );
    assert_eq!(restored[1].images[0].data, "BEFORE_SAVE_DATA");
    assert_eq!(restored[1].images[0].media_type, "image/png");
}

#[test]
fn text_only_history_does_not_serialize_empty_images_field() {
    // Cost optimization: text-only sessions (the 99% case) must NOT
    // bloat their JSONL files with `"images": []` on every message.
    let mut conv = Conversation::new(100_000);
    conv.add_user("hi");
    conv.add_assistant("hello");

    let serialized = serde_json::to_string(conv.messages()).unwrap();
    assert!(
        !serialized.contains("\"images\""),
        "text-only session leaked empty images fields: {}",
        serialized
    );
}
