#!/bin/bash
# Demo environment: a fake NAS (rclone serve webdav) with realistic content,
# mounted by nasyncloud with the interface on http://127.0.0.1:18731.
# Linux only. Needs rclone and FUSE. Run `cargo build` first.
# Stop it with:  pkill -INT -x nasyncloud; pkill -f "rclone serve webdav /tmp/demo"
set -e
BIN=${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/nasyncloud}
D=/tmp/demo; N=$D/nas
fusermount3 -uz $D/mnt 2>/dev/null || true; pkill -x nasyncloud || true; pkill -f "rclone serve webdav $D" || true; sleep 0.5
rm -rf $D; mkdir -p $D/mnt "$N/Documentos/Faturas" "$N/Documentos/Contratos" "$N/Fotos/2026 Férias Algarve" "$N/Fotos/Batizado da Inês" "$N/Vídeos" "$N/Projetos/convite-casamento"
for m in 07 08 09 10; do head -c 180000 /dev/urandom > "$N/Documentos/Faturas/Fatura EDP 2026-$m.pdf"; done
head -c 420000 /dev/urandom > "$N/Documentos/Contratos/Contrato arrendamento.pdf"; head -c 950000 /dev/urandom > "$N/Documentos/IRS 2025.pdf"
for i in $(seq 1 9); do head -c $((1500000 + i*230000)) /dev/urandom > "$N/Fotos/2026 Férias Algarve/IMG_04$((10+i)).jpg"; done
for i in 1 2 3; do head -c 2100000 /dev/urandom > "$N/Fotos/Batizado da Inês/DSC_88$i.jpg"; done
head -c 48000000 /dev/urandom > "$N/Vídeos/Concerto Coliseu.mkv"; head -c 12000000 /dev/urandom > "$N/Vídeos/drone praia.mp4"
echo "<html></html>" > "$N/Projetos/convite-casamento/index.html"
cat > $D/config.toml <<EOT
url = "http://127.0.0.1:5007/"
username = "pedro"
password = "segredo"
mountpoint = "$D/mnt"
cache_dir = "$D/cache"
cache_max_size = "200M"
ui_port = 18731
EOT
(setsid nohup rclone serve webdav $N --dir-cache-time 0s --addr 127.0.0.1:5007 --user pedro --pass segredo > $D/rclone.log 2>&1 < /dev/null &)
sleep 1.5
(setsid nohup "$BIN" --config $D/config.toml mount > $D/nasyncloud.log 2>&1 < /dev/null &)
for i in $(seq 50); do mountpoint -q $D/mnt && break; sleep 0.2; done
"$BIN" --config $D/config.toml pin $D/mnt/Documentos > /dev/null
cat "$D/mnt/Fotos/2026 Férias Algarve/IMG_0411.jpg" "$D/mnt/Fotos/2026 Férias Algarve/IMG_0415.jpg" > /dev/null
dd if="$D/mnt/Vídeos/Concerto Coliseu.mkv" of=/dev/null bs=1M count=3 skip=10 2>/dev/null
cat "$D/mnt/Vídeos/drone praia.mp4" > /dev/null
echo "lista de convidados" > "$D/mnt/Projetos/convite-casamento/convidados.txt"
sleep 3
echo "Demo running. Token: $(python3 -c "import json;print(json.load(open('$D/ui.json'))['token'])")"
echo "Screenshots: xvfb-run -a python3 tools/screenshot.py 18731 <token> docs/screenshots"
