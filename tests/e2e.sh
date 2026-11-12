#!/bin/bash
# Requires: Linux with FUSE, rclone (as the WebDAV server), curl, bc, python3. Run `cargo build` first.
# End-to-end test: real FUSE mount against a real WebDAV server (rclone = Go x/net/webdav).
BIN=${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/nasdrive}
W=/tmp/e2e; NAS=$W/nas; MNT=$W/mnt; CFG=$W/config.toml
fusermount3 -uz $MNT 2>/dev/null; pkill -x nasdrive 2>/dev/null; pkill -x rclone 2>/dev/null; sleep 0.5
rm -rf $W; mkdir -p $NAS/Docs/sub $NAS/Media $MNT
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  ok   $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL $1 ${2:+-- $2}"; }
check(){ local d="$1"; shift; if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d"; fi; }
waitfor(){ local t=$1; shift; for i in $(seq $((t*5))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done; return 1; }
nd()   { $BIN --config $CFG "$@"; }
start_nas(){ setsid nohup rclone serve webdav $NAS --dir-cache-time 0s --addr 127.0.0.1:5005 --user pedro --pass segredo > $W/rclone.log 2>&1 < /dev/null & }
start_nd(){ setsid nohup $BIN --config $CFG mount > $W/nasdrive.log 2>&1 < /dev/null & waitfor 10 mountpoint -q $MNT; }

head -c 40000000 /dev/urandom > $NAS/Media/filme.mkv
echo "relatório" > "$NAS/Docs/relatório final (v2).txt"
echo "nota" > $NAS/Docs/sub/nota.txt
seq 1 20000 > $NAS/Docs/numeros.txt
cat > $CFG <<EOT
url = "http://127.0.0.1:5005"
username = "pedro"
password = "segredo"
mountpoint = "$MNT"
cache_dir = "$W/cache"
dir_ttl_secs = 1
upload_delay_secs = 1
ui_port = 18730
EOT
start_nas; waitfor 10 curl -sf -u pedro:segredo http://127.0.0.1:5005/ || { echo "rclone did not start"; cat $W/rclone.log; exit 1; }

echo "== mount and browse"
start_nd && ok "mounted" || { bad "mount"; cat $W/nasdrive.log; exit 1; }
check "top-level listing" test "$(ls $MNT | tr '\n' ' ')" = "Docs Media "
check "names with accents/spaces" test -f "$MNT/Docs/relatório final (v2).txt"
check "size without download" test "$(stat -c %s $MNT/Media/filme.mkv)" = 40000000
check "online-only file uses no local blocks" test "$(stat -c %b $MNT/Media/filme.mkv)" = 0
grep -q "GET /Media" $W/rclone.log && bad "browsing triggered a download" || ok "browsing downloads nothing"

echo "== on-demand reads"
check "small file content" test "$(cat "$MNT/Docs/relatório final (v2).txt")" = "relatório"
check "partial read is correct" cmp <(dd if=$MNT/Media/filme.mkv bs=1M skip=20 count=1 2>/dev/null) <(dd if=$NAS/Media/filme.mkv bs=1M skip=20 count=1 2>/dev/null)
st=$(nd status $MNT/Media/filme.mkv); echo "$st" | grep -q partial && ok "partially cached after a seek ($(echo $st | sed 's/.*partial *//'))" || bad "status after partial read" "$st"
t0=$(date +%s.%N); sum=$(md5sum < $MNT/Media/filme.mkv); t1=$(date +%s.%N)
check "full read matches (md5)" test "$sum" = "$(md5sum < $NAS/Media/filme.mkv)"
echo "       40 MB read through the mount in $(echo "$t1 - $t0" | bc | cut -c1-5)s"
waitfor 5 sh -c "$BIN --config $CFG status $MNT/Media/filme.mkv | grep -q cached" && ok "fully cached afterwards" || bad "status after full read" "$(nd status $MNT/Media/filme.mkv)"
check "du sees the cached bytes" test "$(du -sm $MNT/Media | cut -f1)" -ge 38

echo "== writes"
cp /etc/services $MNT/Docs/services.txt
waitfor 10 cmp /etc/services $NAS/Docs/services.txt && ok "cp of a new file reaches the NAS" || bad "cp new file"
mkdir -p $MNT/Projetos/app/src && echo 'fn main() {}' > $MNT/Projetos/app/src/main.rs
waitfor 10 grep -qx 'fn main() {}' $NAS/Projetos/app/src/main.rs && ok "mkdir -p + new file" || bad "mkdir -p + new file"
echo "linha extra" >> $MNT/Docs/numeros.txt
waitfor 10 sh -c "tail -1 $NAS/Docs/numeros.txt | grep -q 'linha extra' && test \$(wc -l < $NAS/Docs/numeros.txt) = 20001" && ok "append to an existing file" || bad "append" "$(wc -l < $NAS/Docs/numeros.txt)"
sed -i 's/nota/nota editada/' $MNT/Docs/sub/nota.txt
waitfor 10 grep -qx "nota editada" $NAS/Docs/sub/nota.txt && ok "sed -i (temp file + rename over)" || bad "sed -i" "$(cat $NAS/Docs/sub/nota.txt)"
echo "v2" > "$MNT/Docs/relatório final (v2).txt"
waitfor 10 grep -qx v2 "$NAS/Docs/relatório final (v2).txt" && ok "overwrite with >" || bad "overwrite"
python3 - <<PY && ok "random-access write (seek + write + fsync)" || bad "random-access write"
import os
p="$MNT/Docs/numeros.txt"
f=open(p,"r+b"); f.seek(0); f.write(b"X"); f.flush(); os.fsync(f.fileno()); f.close()
PY
waitfor 10 sh -c "head -c1 $NAS/Docs/numeros.txt | grep -q X" && ok "…and it reaches the NAS" || bad "random-access write upload"
mkdir -p $W/src && for i in $(seq 1 150); do echo "ficheiro $i" > $W/src/f$i.txt; done; mkdir -p $W/src/d1/d2 && cp /etc/hostname $W/src/d1/d2/h
tar -C $W/src -cf - . | tar -C $MNT/Projetos -xf - 2>$W/tar.err
waitfor 40 diff -r $W/src $NAS/Projetos --exclude=app && ok "tar of 150 small files arrives intact" || bad "tar extract" "$(diff -rq $W/src $NAS/Projetos | head -3; cat $W/tar.err | head -3)"
head -c 30000000 /dev/urandom > $W/big.bin; cp $W/big.bin $MNT/Media/big.bin
waitfor 30 cmp $W/big.bin $NAS/Media/big.bin && ok "30 MB upload matches" || bad "big upload"
ls -a $NAS/Media | grep -q nasdrive-part && bad "temporary upload file left behind" || ok "no temporary upload files left"

echo "== rename / delete"
mv $MNT/Docs/services.txt $MNT/Docs/servicos.txt; check "mv file" test -f $NAS/Docs/servicos.txt -a ! -e $NAS/Docs/services.txt
mv $MNT/Projetos/app $MNT/Docs/app; check "mv folder to another folder" test -f $NAS/Docs/app/src/main.rs -a ! -e $NAS/Projetos/app
rm $MNT/Docs/servicos.txt; check "rm" test ! -e $NAS/Docs/servicos.txt
rmdir $MNT/Docs/sub 2>/dev/null && bad "rmdir of a non-empty folder succeeded" || ok "rmdir refuses a non-empty folder"
rm -r $MNT/Projetos; check "rm -r" test ! -e $NAS/Projetos
check "local view agrees" test "$(ls $MNT | tr '\n' ' ')" = "Docs Media "

echo "== pin / free / status"
nd pin $MNT/Docs >/dev/null && waitfor 10 sh -c "$BIN --config $CFG status $MNT/Docs | grep -Eq '([0-9]+)/\1 files local'" && ok "pin downloads the folder: $(nd status $MNT/Docs | cut -d: -f2-)" || bad "pin" "$(nd status $MNT/Docs)"
echo "novo no NAS" > $NAS/Docs/chegou-depois.txt
waitfor 20 sh -c "$BIN --config $CFG status $MNT/Docs/chegou-depois.txt 2>/dev/null | grep -q cached" && ok "a file added on the NAS to a pinned folder gets downloaded" || bad "pinned folder follow-up" "$(nd status $MNT/Docs/chegou-depois.txt 2>&1)"
nd ls $MNT/Docs | head -4 | sed 's/^/       /'
nd free $MNT/Media/filme.mkv | grep -q "freed 38" && ok "free: $(nd status $MNT/Media/filme.mkv | cut -d: -f2-)" || bad "free" "$(nd status $MNT/Media/filme.mkv)"
nd status | sed 's/^/       /'

echo "== web interface"
TOK=$(python3 -c "import json;print(json.load(open('$W/ui.json'))['token'])")
API=http://127.0.0.1:18730
j(){ curl -s -H "x-nasdrive-token: $TOK" "$@"; }
code(){ curl -s -o /dev/null -w '%{http_code}' "$@"; }
check "page is served" sh -c "curl -sf $API/ | grep -q '<title>nasdrive</title>'"
check "API refuses requests without the token" test "$(code $API/api/state)" = 401
check "API refuses a wrong token" test "$(code -H 'x-nasdrive-token: 00' $API/api/state)" = 401
check "API refuses foreign Host headers (DNS rebinding)" test "$(code -H 'Host: evil.example:18730' -H "x-nasdrive-token: $TOK" $API/api/state)" = 421
st=$(j $API/api/state); echo "$st" | python3 -c "import json,sys; s=json.load(sys.stdin); assert s['phase']=='running' and s['info']['online'] and s['configured'] and 'password' not in s['config'], s" && ok "state: running, online, no password exposed" || bad "state" "$st"
ls=$(j "$API/api/browse?path=Docs"); echo "$ls" | python3 -c "import json,sys; l=json.load(sys.stdin); n={e['name']:e for e in l['entries']}; assert n['numeros.txt']['state']=='cached' and n['sub']['dir'] and l['this']['pinned']=='yes', l" && ok "browse lists a folder with states" || bad "browse" "$ls"
check "browse refuses .." test "$(code -H "x-nasdrive-token: $TOK" "$API/api/browse?path=Docs/../..")" = 400
j -X POST -d '{"path":"Media","on":true}' $API/api/pin | grep -q '"ok":true' && waitfor 30 sh -c "$BIN --config $CFG status $MNT/Media | grep -q '2/2 files local'" && ok "pin via the interface downloads the folder" || bad "pin via API" "$(nd status $MNT/Media)"
fr=$(j -X POST -d '{"path":"Media"}' $API/api/free); echo "$fr" | python3 -c "import json,sys; r=json.load(sys.stdin); assert r['freed'] >= 60_000_000, r" && ok "free via the interface ($(echo $fr))" || bad "free via API" "$fr"
t=$(j -X POST -d '{"url":"http://127.0.0.1:5005","username":"pedro","password":"errada"}' $API/api/test); echo "$t" | grep -q 'errados' && ok "connection test reports a wrong password" || bad "test endpoint" "$t"
t=$(j -X POST -d '{"url":"http://127.0.0.1:5005","username":"pedro"}' $API/api/test); echo "$t" | grep -q '"ok":true' && ok "connection test reuses the stored password" || bad "test with stored password" "$t"
t=$(j -X POST -d '{"url":"http://127.0.0.1:5005/Nada","username":"pedro"}' $API/api/test); echo "$t" | grep -q 'não existe' && ok "connection test reports a missing folder" || bad "test missing folder" "$t"
j "$API/api/log?after=0" | grep -q 'uploaded Docs' && ok "activity log is exposed" || bad "log endpoint"
cp $CFG $W/config.before
cfgjson=$(j $API/api/state | python3 -c "import json,sys; c=json.load(sys.stdin)['config']; c['dir_ttl_secs']=2; print(json.dumps({'config': c}))")
j -X POST -d "$cfgjson" $API/api/config | grep -q '"ok":true' && grep -q 'dir_ttl_secs = 2' $CFG && grep -q 'password = "segredo"' $CFG && ok "saving settings keeps the stored password" || bad "save settings" "$(cat $CFG)"
waitfor 20 sh -c "curl -s -H 'x-nasdrive-token: $TOK' $API/api/state | grep -q '\"phase\":\"running\"' && mountpoint -q $MNT" && ok "the mount restarts with the new settings" || bad "restart after save" "$(tail -5 $W/nasdrive.log)"
check "files are still there after the restart" grep -qx "nota editada" $MNT/Docs/sub/nota.txt
j -X POST -d '{}' $API/api/unmount >/dev/null; waitfor 15 sh -c "! mountpoint -q $MNT" && ok "stop from the interface unmounts" || bad "unmount via API"
check "…and the interface keeps answering" sh -c "curl -s -H 'x-nasdrive-token: $TOK' $API/api/state | grep -q '\"phase\":\"stopped\"'"
j -X POST -d '{}' $API/api/mount >/dev/null; waitfor 15 mountpoint -q $MNT && ok "start from the interface mounts again" || bad "mount via API" "$(tail -5 $W/nasdrive.log)"
cp $W/config.before $CFG

echo "== changes made on the NAS by someone else"
echo "alterado no NAS, com outro tamanho" > $NAS/Docs/sub/nota.txt; sleep 1.5
waitfor 8 grep -q "alterado no NAS" $MNT/Docs/sub/nota.txt && ok "remote edit shows up" || bad "remote edit" "$(cat $MNT/Docs/sub/nota.txt)"
rm $NAS/Docs/chegou-depois.txt; sleep 1.5; waitfor 8 test ! -e $MNT/Docs/chegou-depois.txt && ok "remote delete shows up" || bad "remote delete"

echo "== offline"
pkill -x rclone; sleep 0.5
check "pinned file readable offline" grep -q "alterado no NAS" $MNT/Docs/sub/nota.txt
err=$(dd if=$MNT/Media/filme.mkv of=/dev/null bs=4k count=1 2>&1); echo "$err" | grep -qi "host is down" && ok "online-only file fails cleanly: Host is down" || bad "uncached read offline" "$err"
echo "escrito offline" > $MNT/Docs/offline.txt && mkdir $MNT/Docs/pasta-offline && echo x > $MNT/Docs/pasta-offline/x && ok "can create files and folders offline" || bad "offline create"
rm $MNT/Docs/numeros.txt 2>/dev/null && bad "offline delete of a synced file should be refused" || ok "offline delete of a synced file is refused"
nd status | grep -q UNREACHABLE && ok "status reports offline, $(nd status | grep pending | sed 's/pending: *//')" || bad "offline status" "$(nd status)"
start_nas
waitfor 25 sh -c "grep -qx 'escrito offline' $NAS/Docs/offline.txt && test -f $NAS/Docs/pasta-offline/x" && ok "queued changes are sent when the NAS is back" || bad "resume after offline" "$(nd status; tail -5 $W/nasdrive.log)"

echo "== restart"
pkill -INT -x nasdrive; waitfor 15 sh -c '! pgrep -x nasdrive' && ok "Ctrl+C unmounts and exits" || bad "shutdown" "$(tail -5 $W/nasdrive.log)"
check "mountpoint is unmounted and empty" sh -c "! mountpoint -q $MNT && test -z \"\$(ls -A $MNT 2>&1)\""
cp $W/nasdrive.log $W/nasdrive1.log; : > $W/rclone.log; start_nd && ok "mounted again" || bad "second mount" "$(tail -5 $W/nasdrive.log)"
nd status $MNT/Docs | grep -q "pinned" && ok "pins survive a restart" || bad "pin persistence" "$(nd status $MNT/Docs)"
cat $MNT/Docs/numeros.txt > /dev/null; grep -q "GET /Docs/numeros" $W/rclone.log && bad "cached file was downloaded again after restart" || ok "cache survives a restart (no new download)"
pkill -INT -x nasdrive; waitfor 15 sh -c '! pgrep -x nasdrive'; pkill -x rclone
ls $NAS/Docs | grep -q conflict && bad "spurious conflict copy" "$(ls $NAS/Docs)" || ok "no spurious conflict copies"
grep -hE "panicked|ERROR|WARN" $W/nasdrive1.log $W/nasdrive.log | grep -v "unreachable, working offline" | head -8
echo "== $pass passed, $fail failed"
