//! HA-02 / HA-03: a restore that cannot finish must not destroy
//! anything nor report success, and it must give files their modes back.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use x0ne_runtime::config::AppConfig;
use x0ne_runtime::executor::snapshot::{action_create_snapshot, action_restore_snapshot};
use x0ne_runtime::result::ActionOutput;

fn cfg(ws: &Path, store: &Path) -> AppConfig {
    let mut c = AppConfig::defaults();
    c.snapshots.dir = store.to_string_lossy().into_owned();
    c.policy.allowed_write_paths = vec![ws.to_string_lossy().into_owned()];
    c
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[tokio::test]
async fn a_restore_with_a_missing_backup_destroys_nothing_and_fails() {
    let ws = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let c = cfg(ws.path(), store.path());
    let a = ws.path().join("a.txt");
    let b = ws.path().join("b.txt");
    std::fs::write(&a, "A-OLD").unwrap();
    std::fs::write(&b, "B-OLD").unwrap();
    let ws_s = ws.path().to_string_lossy().into_owned();
    assert!(action_create_snapshot("s1", &[ws_s], &c).await.success);

    std::fs::write(&a, "A-NEW").unwrap();
    std::fs::write(&b, "B-NEW").unwrap();
    std::fs::write(ws.path().join("c.txt"), "C-ADDED").unwrap();
    // Lose one backup: every .dat but the first goes.
    let snap = store.path().join("s1");
    let mut dats: Vec<_> = std::fs::read_dir(&snap)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "dat"))
        .collect();
    dats.sort();
    std::fs::remove_file(dats.last().unwrap()).unwrap();

    let r = action_restore_snapshot("s1", &c).await;
    assert!(
        !r.success,
        "a restore that lost a file reported success: {r:?}"
    );
    for (p, want) in [(&a, ["A-OLD", "A-NEW"]), (&b, ["B-OLD", "B-NEW"])] {
        let got = std::fs::read_to_string(p).unwrap_or_else(|_| "<deleted>".into());
        assert!(
            want.contains(&got.as_str()),
            "{} is now {got:?}",
            p.display()
        );
    }
    assert!(
        ws.path().join("c.txt").exists(),
        "cleanup deleted a file although the restore failed"
    );
}

#[tokio::test]
async fn a_restore_gives_files_their_modes_back() {
    let ws = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let store = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let c = cfg(ws.path(), store.path());
    let script = ws.path().join("run.sh");
    let secret = ws.path().join("token");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(&secret, "s3cr3t").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
    let paths = vec![
        script.to_string_lossy().into_owned(),
        secret.to_string_lossy().into_owned(),
    ];
    assert!(action_create_snapshot("s2", &paths, &c).await.success);
    for f in std::fs::read_dir(store.path().join("s2")).unwrap() {
        let p = f.unwrap().path();
        if p.extension().is_some_and(|x| x == "dat") {
            assert_eq!(
                mode(&p),
                0o600,
                "backup {} is readable by others",
                p.display()
            );
        }
    }

    std::fs::write(&script, "broken").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(&secret).unwrap();

    let r = action_restore_snapshot("s2", &c).await;
    assert!(r.success, "{r:?}");
    assert!(matches!(
        r.output,
        Some(ActionOutput::SnapshotRestored {
            files_failed: 0,
            ..
        })
    ));
    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
    assert_eq!(mode(&script), 0o755, "the script lost its exec bit");
    assert_eq!(
        mode(&secret),
        0o600,
        "the secret came back wider than it was"
    );
}
