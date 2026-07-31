#!/usr/bin/env bash
# Ship this tree to the box, build there, and restart the engine.
#
#   ./engine/deploy.sh            # sync, build, restart
#   ./engine/deploy.sh --no-restart
#
# Run it from the repo root or from engine/; either works.
set -euo pipefail

BOX="${DUBU_BOX:-ubuntu@162.19.94.9}"
DEST="/home/luke/dubu/engine"
STAGE="/tmp/dubu-engine-src"
UNIT="solana-bot"

# Files that live ONLY on the box. Syncing with --delete and without these removed `run.sh` on
# 2026-07-31 and the engine spent three minutes in a 203/EXEC restart loop before anyone noticed:
# systemd reported `activating`, not `failed`, because Restart=on-failure kept it trying.
#
# `state/` is the killswitch latch, `.env` holds the key, `updater.toml` is tuned live, and the
# rollback binaries are the only copies of what was running before.
KEEP=(
  --exclude 'target/'
  --exclude '.env'
  --exclude 'updater.toml'
  --exclude 'state/'
  --exclude '*.log'
  --exclude 'run.sh'
  --exclude 'dubu-updater.rollback*'
)

cd "$(dirname "$0")"
sha="$(git rev-parse --short HEAD)"
dirty="$(git status --porcelain -- . | head -1)"
[[ -n "$dirty" ]] && echo "warning: deploying a dirty tree (uncommitted changes under engine/)" >&2

echo "==> staging $sha to $BOX"
rsync -az --delete "${KEEP[@]}" ./ "$BOX:$STAGE/"
ssh "$BOX" "sudo -n rsync -a --delete ${KEEP[*]} --chown=luke:luke $STAGE/ $DEST/"

echo "==> building on the box"
ssh "$BOX" "sudo -n -u luke bash -lc 'cd $DEST && PATH=/home/luke/.cargo/bin:\$PATH cargo build --release -p dubu-updater'"

if [[ "${1:-}" == "--no-restart" ]]; then
  echo "==> built, not restarting"
  exit 0
fi

# The running process still holds the previous binary's inode even though cargo has replaced the
# path, so this is the last moment it can be copied out.
echo "==> preserving the running binary"
ssh "$BOX" "PID=\$(pgrep -f 'release/dubu-updater --config' | head -1); \
  if [[ -n \"\$PID\" ]]; then \
    sudo -n cp /proc/\$PID/exe $DEST/dubu-updater.rollback-running && \
    sudo -n chown luke:luke $DEST/dubu-updater.rollback-running; \
  else echo '(nothing running)'; fi"

echo "==> restarting $UNIT"
ssh "$BOX" "sudo -n systemctl restart $UNIT && sleep 20 && \
  sudo -n systemctl show $UNIT -p ActiveState -p SubState -p ExecMainStartTimestamp -p NRestarts"

# `activating` is not success: a unit stuck in an exec loop reports exactly that.
state="$(ssh "$BOX" "sudo -n systemctl show $UNIT -p ActiveState --value")"
if [[ "$state" != "active" ]]; then
  echo "FAILED: $UNIT is '$state', not 'active'" >&2
  ssh "$BOX" "sudo -n journalctl -u $UNIT -n 20 --no-pager" >&2
  exit 1
fi
echo "==> $sha is live"
