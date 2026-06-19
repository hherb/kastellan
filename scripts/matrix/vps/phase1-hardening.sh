#!/usr/bin/env bash
# =============================================================================
# Phase 1 — OS baseline hardening for the kastellan Matrix VPS
# Host: matrix.kastellan.dev   (Ubuntu 26.04, 1 GB RAM, SSH on 2222)
#
# Run as root:   sudo bash phase1-hardening.sh
#
# Idempotent — safe to re-run. Touches ONLY the OS baseline (swap, firewall,
# fail2ban, auto-updates, SSH/sysctl hardening). It does NOT install or touch
# the Matrix homeserver — that is Phase 2.
#
# SAFETY: every step that could affect remote access (ufw, sshd) is ordered so
# the current SSH session on port 2222 is never dropped:
#   * ufw allows 2222/tcp BEFORE the firewall is enabled,
#   * sshd config is validated with `sshd -t` BEFORE any reload,
#   * a reload only affects NEW connections; your live session stays up.
# =============================================================================
set -euo pipefail

SSH_PORT=2222

log() { printf '\n=== %s ===\n' "$*"; }
if [ "$(id -u)" -ne 0 ]; then echo "Run as root (sudo bash $0)"; exit 1; fi

# -----------------------------------------------------------------------------
# 1. Swap — this is a 1 GB box with no swap; RocksDB (Phase 2) needs headroom.
# -----------------------------------------------------------------------------
log "Swap"
if swapon --show | grep -q .; then
  echo "swap already present:"; swapon --show
else
  fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048 status=none
  chmod 600 /swapfile
  mkswap /swapfile >/dev/null
  swapon /swapfile
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
  echo "2 GB swap added + persisted in /etc/fstab"
fi

# -----------------------------------------------------------------------------
# 2. sysctl — low swappiness + conservative network hardening.
# -----------------------------------------------------------------------------
log "sysctl hardening"
cat > /etc/sysctl.d/99-kastellan.conf <<'EOF'
vm.swappiness = 10
net.ipv4.conf.all.rp_filter = 1
net.ipv4.conf.default.rp_filter = 1
net.ipv4.icmp_echo_ignore_broadcasts = 1
net.ipv4.conf.all.accept_redirects = 0
net.ipv6.conf.all.accept_redirects = 0
net.ipv4.conf.all.send_redirects = 0
net.ipv4.conf.all.accept_source_route = 0
net.ipv6.conf.all.accept_source_route = 0
net.ipv4.tcp_syncookies = 1
kernel.kptr_restrict = 2
EOF
sysctl --system >/dev/null
echo "applied"

# -----------------------------------------------------------------------------
# 3. UFW — default-deny inbound; allow SSH(2222) + HTTP(80, ACME) + HTTPS(443).
#    The Matrix federation port 8448 is intentionally NEVER opened.
# -----------------------------------------------------------------------------
log "UFW firewall"
apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ufw >/dev/null
ufw allow "${SSH_PORT}/tcp" comment 'SSH'        >/dev/null
ufw allow 80/tcp  comment 'HTTP (ACME challenge)' >/dev/null
ufw allow 443/tcp comment 'HTTPS (Matrix client API)' >/dev/null
ufw default deny incoming  >/dev/null
ufw default allow outgoing >/dev/null
ufw --force enable
ufw status verbose

# -----------------------------------------------------------------------------
# 4. fail2ban — ban brute-forcers against SSH on 2222 (reads the systemd journal).
# -----------------------------------------------------------------------------
log "fail2ban"
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq fail2ban >/dev/null
install -d /etc/fail2ban/jail.d
cat > /etc/fail2ban/jail.d/sshd.local <<EOF
[sshd]
enabled  = true
port     = ${SSH_PORT}
backend  = systemd
maxretry = 5
findtime = 10m
bantime  = 1h
EOF
systemctl enable fail2ban >/dev/null 2>&1 || true
systemctl restart fail2ban
sleep 1
fail2ban-client status sshd || true

# -----------------------------------------------------------------------------
# 5. unattended-upgrades — already installed; make sure it is enabled.
# -----------------------------------------------------------------------------
log "unattended-upgrades"
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq unattended-upgrades >/dev/null
cat > /etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF
systemctl enable unattended-upgrades >/dev/null 2>&1 || true
systemctl restart unattended-upgrades >/dev/null 2>&1 || true
echo "enabled (daily package-list update + unattended security upgrades)"

# -----------------------------------------------------------------------------
# 6. SSH hardening — additive drop-in. Box is already key-only + root-login off
#    (BinaryLane's 10-binarylane.conf); this re-asserts that and tightens limits.
#    Validated with `sshd -t` before any reload, so a typo can never lock you out.
# -----------------------------------------------------------------------------
log "SSH hardening"
cat > /etc/ssh/sshd_config.d/20-kastellan-hardening.conf <<EOF
# kastellan hardening — additive to the BinaryLane 10-binarylane.conf drop-in.
PasswordAuthentication no
PermitRootLogin no
KbdInteractiveAuthentication no
MaxAuthTries 3
LoginGraceTime 30
X11Forwarding no
AllowAgentForwarding no
AllowTcpForwarding no
ClientAliveInterval 300
ClientAliveCountMax 2
EOF
sshd -t   # aborts the script (set -e) if the merged config is invalid
systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true
echo "sshd config valid + reloaded (new connections only; current session safe)"

# -----------------------------------------------------------------------------
# Summary
# -----------------------------------------------------------------------------
log "Phase 1 complete — effective state"
echo "-- swap --";  swapon --show
echo "-- sshd (effective) --"; sshd -T 2>/dev/null | grep -iE '^(port|passwordauthentication|permitrootlogin|maxauthtries) '
echo "-- ufw --";   ufw status verbose | sed -n '1,12p'
echo "-- fail2ban --"; systemctl is-active fail2ban
echo
echo "Phase 1 done. Next: Phase 2 (Matrix homeserver install)."
