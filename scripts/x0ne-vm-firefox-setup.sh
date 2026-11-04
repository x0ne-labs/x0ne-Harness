#!/usr/bin/env bash
# x0ne-vm-firefox-setup — bring up a Debian VM that auto-installs X11 +
# openbox + Firefox via cloud-init, then opens virt-viewer so you can watch
# the install + first Firefox launch live.
#
# After Firefox is up, the agent can drive it through the existing 16 MCP
# tools (`vm_screenshot`, `vm_click`, `vm_type`, `vm_key`, ...).
#
# One-shot — re-run after `make x0ne-vm-firefox-destroy` if you want a fresh
# install.

set -euo pipefail

VM_NAME=${VM_NAME:-x0ne-firefox}
URL=${URL:-https://x0ne.co}
RAM_MB=${RAM_MB:-2048}
VCPUS=${VCPUS:-2}
DISK_SIZE=${DISK_SIZE:-6G}

LIBVIRT_URI="qemu:///session"
ISO_DIR="${HOME}/.x0ne/vm/iso"
DISK_DIR="${HOME}/.x0ne/vm/disks"
LOG_DIR="${HOME}/.x0ne/vm/logs"
CIDATA_DIR="${HOME}/.x0ne/vm/cidata/${VM_NAME}"

SRC_IMG="${ISO_DIR}/debian-12-nocloud-amd64.qcow2"
DEST_DISK="${DISK_DIR}/${VM_NAME}.qcow2"

C_OK="\033[32m✓\033[0m"
C_STEP="\033[1;36m▶\033[0m"

step() { printf "\n${C_STEP} %s\n" "$*"; }
ok()   { printf "  ${C_OK} %s\n" "$*"; }

# ─── 1. prereqs ─────────────────────────────────────────────────────────────

step "checking prereqs"
for c in virsh virt-install qemu-img; do
    command -v $c >/dev/null || { echo "  ✗ $c missing"; exit 1; }
done
[[ -f "$SRC_IMG" ]] || { echo "  ✗ source image missing: $SRC_IMG"; echo "    grab it with: curl -L -o $SRC_IMG https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-nocloud-amd64.qcow2"; exit 1; }
ok "all prereqs present"

# ─── 2. prepare disk (copy + resize) ───────────────────────────────────────

step "preparing disk"
mkdir -p "$DISK_DIR" "$LOG_DIR" "$CIDATA_DIR"
if [[ -f "$DEST_DISK" ]]; then
    ok "reusing existing disk: $DEST_DISK"
else
    cp --reflink=auto "$SRC_IMG" "$DEST_DISK"
    qemu-img resize "$DEST_DISK" "$DISK_SIZE" >/dev/null
    ok "copied + resized to $DISK_SIZE → $DEST_DISK"
fi

# ─── 3. write cloud-init user-data ─────────────────────────────────────────

step "writing cloud-init user-data"
cat > "$CIDATA_DIR/user-data" <<EOF
#cloud-config
hostname: ${VM_NAME}

# Create an unprivileged user with a known password (sudo NOPASSWD).
users:
  - default
  - name: agent
    groups: [sudo, video]
    plain_text_passwd: agent
    lock_passwd: false
    shell: /bin/bash
    sudo: 'ALL=(ALL) NOPASSWD:ALL'

# Disable auto-upgrade — we want install to complete fast.
package_update: true
package_upgrade: false

# What to install. Order matters slightly — display server first.
packages:
  - xserver-xorg-core
  - xserver-xorg-input-libinput
  - xserver-xorg-video-fbdev
  - xserver-xorg-video-qxl
  - xinit
  - openbox
  - firefox-esr
  - x11-xserver-utils
  - x11-utils
  - unclutter

write_files:
  # Auto-login agent on tty1.
  - path: /etc/systemd/system/getty@tty1.service.d/autologin.conf
    content: |
      [Service]
      ExecStart=
      ExecStart=-/sbin/agetty --autologin agent --noclear %I \$TERM
    permissions: '0644'

  # When agent logs in on tty1, immediately start X.
  - path: /home/agent/.bash_profile
    owner: agent:agent
    permissions: '0644'
    content: |
      if [ -z "\$DISPLAY" ] && [ "\$XDG_VTNR" = 1 ]; then
        exec startx -- vt1 -nolisten tcp 2> /tmp/xorg.log
      fi

  # Bare X session: openbox + a screenshot-friendly cursor.
  - path: /home/agent/.xinitrc
    owner: agent:agent
    permissions: '0755'
    content: |
      #!/bin/sh
      xsetroot -solid '#1e2030'
      unclutter -idle 0 &
      exec openbox-session

  # Openbox autostart spawns Firefox with a clean profile pointed at the URL.
  - path: /home/agent/.config/openbox/autostart
    owner: agent:agent
    permissions: '0755'
    content: |
      #!/bin/sh
      # Wait a beat so X is fully up, then launch Firefox.
      sleep 1
      exec firefox-esr --new-window --no-remote \\
        --profile /home/agent/.mozilla/firefox/x0ne \\
        '${URL}'

  # Minimal openbox config — full-screen Firefox, no decoration.
  - path: /home/agent/.config/openbox/rc.xml
    owner: agent:agent
    permissions: '0644'
    content: |
      <?xml version="1.0" encoding="UTF-8"?>
      <openbox_config xmlns="http://openbox.org/3.4/rc">
        <applications>
          <application class="*">
            <decor>no</decor>
            <maximized>yes</maximized>
          </application>
        </applications>
      </openbox_config>

  # Firefox prefs: skip first-run, no telemetry banners.
  - path: /home/agent/.mozilla/firefox/x0ne/user.js
    owner: agent:agent
    permissions: '0644'
    content: |
      user_pref("browser.shell.checkDefaultBrowser", false);
      user_pref("browser.startup.homepage_override.mstone", "ignore");
      user_pref("browser.startup.firstrunSkipsHomepage", true);
      user_pref("trailhead.firstrun.didSeeAboutWelcome", true);
      user_pref("datareporting.policy.dataSubmissionPolicyAcceptedVersion", 2);
      user_pref("datareporting.healthreport.uploadEnabled", false);

runcmd:
  - [ mkdir, -p, /home/agent/.config/openbox, /home/agent/.mozilla/firefox/x0ne ]
  - [ chown, -R, 'agent:agent', /home/agent/.config, /home/agent/.mozilla ]
  - [ systemctl, daemon-reload ]
  - [ systemctl, restart, getty@tty1 ]

final_message: "x0ne-firefox ready — Firefox should autostart on tty1"
EOF

cat > "$CIDATA_DIR/meta-data" <<EOF
instance-id: ${VM_NAME}
local-hostname: ${VM_NAME}
EOF
ok "user-data + meta-data in $CIDATA_DIR"

# ─── 4. define + start the VM ──────────────────────────────────────────────

step "defining VM '$VM_NAME' via virt-install --import"

if virsh -c "$LIBVIRT_URI" list --all --name | grep -qx "$VM_NAME"; then
    ok "VM already defined — starting it"
    virsh -c "$LIBVIRT_URI" start "$VM_NAME" 2>/dev/null || true
else
    virt-install \
        --connect "$LIBVIRT_URI" \
        --name "$VM_NAME" \
        --memory "$RAM_MB" \
        --vcpus "$VCPUS" \
        --import \
        --os-variant debian12 \
        --disk path="$DEST_DISK",format=qcow2,bus=virtio \
        --network user,model=virtio \
        --graphics vnc,listen=127.0.0.1 \
        --video qxl \
        --cloud-init user-data="$CIDATA_DIR/user-data",meta-data="$CIDATA_DIR/meta-data" \
        --noautoconsole \
        --wait 0 \
        > "$LOG_DIR/virt-install-${VM_NAME}.log" 2>&1
    ok "VM defined and starting"
fi

# ─── 5. find VNC port & launch virt-viewer ─────────────────────────────────

step "looking up VNC endpoint"
sleep 2
vnc=$(virsh -c "$LIBVIRT_URI" vncdisplay "$VM_NAME" 2>/dev/null | head -1 | tr -d '[:space:]' || true)
if [[ -n "$vnc" ]]; then
    ok "VNC: $vnc → 127.0.0.1:$((5900 + ${vnc##*:}))"
else
    ok "(VNC not ready yet — VM may still be bootstrapping)"
fi

# ─── 6. instructions ────────────────────────────────────────────────────────

cat <<EOF

════════════════════════════════════════════════════════════════════
 ${C_OK} VM '${VM_NAME}' is bootstrapping.

 Cloud-init will:
   1. Update apt index   (~30s)
   2. Install xorg + openbox + firefox-esr  (~3-5 min, ~250MB)
   3. Restart tty1 → agent autologin → startx → Firefox opens

 Watch the install live with:
   virt-viewer --connect ${LIBVIRT_URI} ${VM_NAME}

 Or via the x0ne watcher (browser-based, polled screenshots):
   VM_NAME=${VM_NAME} bash scripts/x0ne-vm-watch.sh

 Once Firefox is up, drive it via MCP:
   X0NE_VM_BACKEND=libvirt x0ne-vm-mcp
   → vm_screenshot / vm_click / vm_type / vm_key on '${VM_NAME}'

 To destroy + start fresh:
   virsh -c ${LIBVIRT_URI} destroy ${VM_NAME}
   virsh -c ${LIBVIRT_URI} undefine ${VM_NAME} --remove-all-storage
   rm -rf ${CIDATA_DIR}
   bash $0
════════════════════════════════════════════════════════════════════
EOF
