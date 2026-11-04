//! Integration tests for the pool state machine + XML rewrite pipeline.
//!
//! These don't touch libvirt — they exercise pure logic that the daemon
//! and the LibvirtSession depend on. Real-VM tests live in
//! `scripts/x0ne-vm-control-test.sh` (the kitchen-sink) and
//! `scripts/x0ne-vm-six-agents.sh` (the multi-agent demo).

use x0ne_vm::pool::{PoolConfig, PoolState, SlotState};
use x0ne_vm::virsh::{parse_first_disk_path, rewrite_xml_for_clone, strip_xml_disk_with_device};

// ─── pool state machine ────────────────────────────────────────────────────

#[test]
fn full_pool_lifecycle_through_six_slots() {
    let mut p = PoolState::new(PoolConfig {
        base: "alpine-golden".into(),
        target: 6,
        prefix: "pool".into(),
        max: 16,
    });

    // Initially empty → deficit equals target
    assert_eq!(p.deficit(), 6);
    assert_eq!(p.free(), 0);
    assert_eq!(p.acquired(), 0);

    // Fill the pool
    for i in 1..=6 {
        let name = format!("pool-{i}");
        p.add_free(&name, 1000 + i as u64);
    }
    assert_eq!(p.free(), 6);
    assert_eq!(p.deficit(), 0);

    // 6 agents acquire one each in order
    let mut acquired_names = Vec::new();
    for i in 1..=6 {
        let agent = format!("agent-{i}");
        let slot = p.acquire(&agent, 2000 + i as u64).unwrap();
        acquired_names.push(slot.name.clone());
    }
    assert_eq!(p.free(), 0);
    assert_eq!(p.acquired(), 6);
    // Pool order is deterministic
    assert_eq!(
        acquired_names,
        vec!["pool-1", "pool-2", "pool-3", "pool-4", "pool-5", "pool-6"]
    );

    // 7th acquire fails — pool is empty
    assert!(p.acquire("agent-7", 9999).is_none());

    // Release pool-3 → goes through recycle → free
    p.start_recycle("pool-3").unwrap();
    assert!(matches!(p.slots["pool-3"].state, SlotState::Recycling));
    p.finish_recycle("pool-3").unwrap();
    assert!(matches!(p.slots["pool-3"].state, SlotState::Free));
    assert_eq!(p.free(), 1);
    assert_eq!(p.acquired(), 5);
}

#[test]
fn agent_can_be_attributed_to_slot() {
    let mut p = PoolState::new(PoolConfig {
        base: "alpine-golden".into(),
        target: 2,
        prefix: "pool".into(),
        max: 8,
    });
    p.add_free("pool-1", 1);
    p.add_free("pool-2", 2);

    p.acquire("alice", 10).unwrap();
    p.acquire("bob", 11).unwrap();

    // Inspect the attribution
    match &p.slots["pool-1"].state {
        SlotState::Acquired { by, .. } => assert_eq!(by, "alice"),
        _ => panic!("pool-1 should be acquired by alice"),
    }
    match &p.slots["pool-2"].state {
        SlotState::Acquired { by, .. } => assert_eq!(by, "bob"),
        _ => panic!("pool-2 should be acquired by bob"),
    }
}

#[test]
fn reaped_name_is_reused_for_next_clone() {
    let mut p = PoolState::new(PoolConfig {
        base: "alpine-golden".into(),
        target: 3,
        prefix: "pool".into(),
        max: 8,
    });
    p.add_free("pool-1", 1);
    p.add_free("pool-2", 2);
    p.add_free("pool-3", 3);
    p.reap("pool-2");
    assert_eq!(p.next_name().as_deref(), Some("pool-2"));
}

// ─── XML rewrite — the actual clone XML pipeline used in production ───────

const BASE_XML: &str = r#"<domain type='kvm'>
  <name>alpine-golden</name>
  <uuid>11111111-2222-3333-4444-555555555555</uuid>
  <memory unit='KiB'>262144</memory>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2'/>
      <source file='/home/u/.x0ne/vm/disks/alpine-golden.qcow2' index='1'/>
      <backingStore/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <source file='/some/install.iso'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <interface type='user'>
      <mac address='52:54:00:de:ad:be'/>
      <model type='virtio'/>
    </interface>
  </devices>
</domain>"#;

#[test]
fn realistic_clone_xml_is_well_formed_for_libvirt() {
    let out = rewrite_xml_for_clone(BASE_XML, "pool-7", "/disks/pool-7.qcow2")
        .expect("rewrite must succeed on realistic XML");

    // Identity
    assert!(out.contains("<name>pool-7</name>"));
    assert!(out.contains("/disks/pool-7.qcow2"));
    assert!(
        !out.contains("/disks/alpine-golden.qcow2"),
        "base disk path must be replaced"
    );
    assert!(
        !out.contains("11111111-2222"),
        "base UUID must be stripped (libvirt assigns new)"
    );

    // CDROM is gone — critical for clones from a base that used `--cdrom`
    assert!(
        !out.contains("device='cdrom'"),
        "cdrom block must be stripped from clone"
    );
    assert!(
        !out.contains("/some/install.iso"),
        "cdrom ISO source must be gone"
    );

    // MAC stripped, model preserved
    assert!(!out.contains("52:54:00:de:ad:be"), "MAC must be stripped");
    assert!(
        out.contains("<model type='virtio'/>"),
        "network model must survive strip"
    );

    // Primary disk preserved (the one we want to boot from)
    assert!(out.contains("<target dev='vda' bus='virtio'/>"));
}

#[test]
fn first_disk_path_finds_primary_disk_not_cdrom() {
    let p = parse_first_disk_path(BASE_XML).expect("must find a disk");
    assert!(
        p.ends_with("alpine-golden.qcow2"),
        "first disk path should be the bootable disk, got: {p}"
    );
}

#[test]
fn strip_cdrom_preserves_other_disks() {
    let xml = r#"<devices>
        <disk type='file' device='disk'><source file='/a.qcow2'/></disk>
        <disk type='file' device='cdrom'><source file='/b.iso'/></disk>
        <disk type='file' device='disk'><source file='/c.qcow2'/></disk>
    </devices>"#;
    let out = strip_xml_disk_with_device(xml, "cdrom");
    assert!(out.contains("/a.qcow2"));
    assert!(out.contains("/c.qcow2"));
    assert!(!out.contains("/b.iso"));
    assert_eq!(out.matches("<disk").count(), 2);
}
