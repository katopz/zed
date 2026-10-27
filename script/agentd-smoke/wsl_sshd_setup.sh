#!/bin/sh
# Per-user sshd for Zed Work Remote (plan 037 P3) inside WSL. Runs only in
# inetd mode (`sshd -i`) through a ProxyCommand, so nothing listens on any
# port and no root or system config is touched. Everything lives in ~/.zed-sshd.
#
# Usage, inside the WSL distro: sh wsl_sshd_setup.sh <public key file to authorize>
set -eu
dir="$HOME/.zed-sshd"
mkdir -p "$dir" "$HOME/.ssh"
chmod 700 "$dir" "$HOME/.ssh"
[ -f "$dir/host_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -C zed-wsl-host -f "$dir/host_ed25519"
cat > "$dir/sshd_config" <<CONFIG
HostKey $dir/host_ed25519
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile $HOME/.ssh/authorized_keys
PidFile none
AllowUsers $(id -un)
Subsystem sftp /usr/lib/openssh/sftp-server
CONFIG
touch "$HOME/.ssh/authorized_keys"
chmod 600 "$HOME/.ssh/authorized_keys"
key="$(cat "$1")"
grep -qxF "$key" "$HOME/.ssh/authorized_keys" || printf '%s\n' "$key" >> "$HOME/.ssh/authorized_keys"
/usr/sbin/sshd -t -f "$dir/sshd_config" && echo "sshd config ok"
ssh-keygen -lf "$dir/host_ed25519.pub"
