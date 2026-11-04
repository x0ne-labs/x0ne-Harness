#!/usr/bin/env python3
"""Harvest X0NE session user turns for corpus B (the asm-ai emission bench).

Session turns are what a person typed, so reading them takes an explicit
`--from-my-sessions`; without it the script refuses and reads nothing.
Harvested rows carry `author: "harvest"` and stay local: the committed
fixture holds none (`tests/corpus_b.rs` checks it). `--supplement` merges a
hand-written JSONL (each row carries `author`) to fill the strata; those rows
are written BEFORE any model call and stay marked until the owner reviews them
(`reviewed_by_owner: true`).

Usage:
  scripts/corpus_b_harvest.py --from-my-sessions --out /tmp/corpus_b.local.jsonl \
      --supplement x0ne-orchestrator/tests/fixtures/corpus_b.supplement.jsonl
  # optional paraphrase by a model DIFFERENT from the one under test (Ollama):
  scripts/corpus_b_harvest.py --paraphrase --model qwen3:8b --in <raw> --out <final>
"""
import argparse
import glob
import json
import os
import pathlib
import random
import sys
import urllib.request

SESSIONS = os.path.expanduser("~/.x0ne/sessions/*.jsonl")

# The four strata of the spec. The assignment is proposed by keyword and
# marked as such; the owner's review is what makes it final.
STRATA = {"easy": 20, "awkward": 15, "out_of_grammar": 10, "adversarial": 5}

# Session turns that are not a task at all (greetings, mood, "continue").
# They are kept out: an utterance with no task to refuse or to do measures
# nothing on any stratum.
CHATTER = ("how do you feel", "what's up", "whats up", "whatsss", "yo ", "yooo", "lol",
           "joke", "lucid", "bro", "buddy", "perfect", "thanks", "continue")


def harvest():
    seen, out = set(), []
    for path in sorted(glob.glob(SESSIONS)):
        for line in open(path, errors="replace"):
            try:
                d = json.loads(line)
            except json.JSONDecodeError:
                continue
            if d.get("type") != "user_input":
                continue
            text = (d.get("content") or "").strip()
            if len(text) < 25 or text.lower() in seen:
                continue
            if text.startswith("{"):
                continue  # pasted terminal cells, not an utterance
            seen.add(text.lower())
            out.append({"source_file": pathlib.Path(path).name, "utterance": text,
                        "author": "harvest"})
    return out


def is_chatter(text):
    t = text.lower()
    return any(w in t for w in CHATTER)


def propose_stratum(text):
    t = text.lower()
    if any(w in t for w in ("ssh", "id_rsa", "password", "secret", "../", "/etc/")):
        return "adversarial"
    if any(w in t for w in ("deploy", "install", "commit", "push", "docker", "vm",
                            "reset", "run ", "launch", "website", "game", "screen")):
        return "out_of_grammar"
    if any(w in t for w in ("regex", "escape", "count", "how many", "filter", "only the")):
        return "awkward"
    return "easy"


def paraphrase(text, model, endpoint):
    prompt = (
        "Rewrite this developer request as one clear English sentence describing "
        "the task, without naming any tool or command. Keep the intent exactly.\n\n"
        f"{text}"
    )
    body = json.dumps({"model": model, "prompt": prompt, "stream": False}).encode()
    req = urllib.request.Request(f"{endpoint}/api/generate", data=body,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.load(r)["response"].strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--in", dest="inp")
    ap.add_argument("--supplement")
    # Measured 2026-10-06: no harvested turn is a file task; only the
    # out_of_grammar ones (reset, make a game, start a website) are real tasks.
    ap.add_argument("--harvest-strata", default="easy,awkward,out_of_grammar,adversarial")
    ap.add_argument("--from-my-sessions", action="store_true",
                    help="read the user turns of ~/.x0ne/sessions (your own messages)")
    ap.add_argument("--paraphrase", action="store_true")
    ap.add_argument("--model", default="qwen3:8b")
    ap.add_argument("--endpoint", default="http://localhost:11434")
    ap.add_argument("--seed", type=int, default=20260815)
    a = ap.parse_args()

    if a.paraphrase:
        rows = [json.loads(l) for l in open(a.inp) if l.strip()]
        for r in rows:
            r["utterance"] = paraphrase(r["utterance"], a.model, a.endpoint)
            r["paraphrased_by"] = a.model
    else:
        if not a.from_my_sessions:
            print("refusing to read ~/.x0ne/sessions: the turns are real user messages. "
                  "Pass --from-my-sessions to consent, and keep the output out of the repository.",
                  file=sys.stderr)
            return 2
        rows = [r for r in harvest() if not is_chatter(r["utterance"])]
        random.Random(a.seed).shuffle(rows)
        keep = set(a.harvest_strata.split(","))
        prefixes = set()
        for r in rows:
            r["stratum"] = propose_stratum(r["utterance"])
            r["stratum_assignment"] = "proposed"
            r["reviewed_by_owner"] = False
        # Near-duplicates (same first 60 chars) count once.
        rows = [r for r in rows if r["stratum"] in keep
                and not (r["utterance"][:60] in prefixes or prefixes.add(r["utterance"][:60]))]
        if a.supplement:
            for l in open(a.supplement):
                if l.strip():
                    r = json.loads(l)
                    r.setdefault("author", "implementer")
                    r.setdefault("stratum_assignment", "proposed")
                    r.setdefault("reviewed_by_owner", False)
                    rows.append(r)
        picked, counts = [], {k: 0 for k in STRATA}
        for r in rows:
            s = r["stratum"]
            if counts[s] < STRATA[s]:
                picked.append(r)
                counts[s] += 1
        missing = {k: STRATA[k] - counts[k] for k in STRATA if counts[k] < STRATA[k]}
        if missing:
            print(f"incomplete strata: {missing}", file=sys.stderr)
        rows = picked

    with open(a.out, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"{len(rows)} statements -> {a.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
