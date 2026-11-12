# Changelog

## Por lançar

- **macOS funciona e está testado** (macOS 27, Apple Silicon, macFUSE 5.5, motor kext): 28 testes e 63 verificações num mount real.
- macOS: o mount passa a ser feito com `fuse_mount` + `Session::from_fd`, porque o `fuse_mount_compat25` usado pelo `fuser` 0.18 falha sempre no macFUSE 5.
- macOS: alterações feitas na NAS por outros passam a aparecer. `getattr`/`open` releem a pasta quando passou o `dir_ttl` (o macFUSE não repete o `lookup`), e o `open` pede ao kernel que largue as páginas em cache (`FOPEN_PURGE_UBC`).
- `tests/e2e.sh` corre em Linux e macOS, com o rclone ou com a NAS falsa `examples/fake_nas.rs` (dav-server), e devolve erro se alguma verificação falhar.
- Pacotes prontos a instalar: `.dmg` para macOS (`nasdrive.app`, que corre em segundo plano, com ícone; avisa se faltar o macFUSE), `.zip` com o `nasdrive.exe` para Windows e `.tar.gz` para Linux. Gerados pelo GitHub Actions (`release.yml`) a partir de uma tag `v*`, que também publica a Release. `tools/package-macos.sh` gera o `.dmg` localmente.
- Interface: botão «Sair», que desmonta a pasta e encerra o nasdrive depois de terminar os envios pendentes.

## 0.1.0 — 2026-11-12

- Cliente WebDAV (PROPFIND, GET por intervalos, PUT, MKCOL, MOVE, DELETE, quota), com contornos para servidores que ignoram `Range` ou respondem 405 a MKCOL.
- Motor Linux/macOS (FUSE): ficheiros a pedido com cache por intervalos, manter offline, libertar espaço, limite de cache com LRU, uploads em segundo plano, trabalho offline com fila persistente, conflitos com cópia, índice com recuperação após crash.
- Motor Windows (Cloud Files API): placeholders, hidratação a pedido, envio de alterações locais, espelho de apagar/renomear, sondagem da NAS. Compila; ainda não testado.
- Linha de comandos: `ui`, `setup`, `mount`, `pin`, `unpin`, `free`, `status`, `ls`, `refresh`, `unmount`, `unregister`.
- Interface web local (`nasdrive ui`): estado, explorador com estado por ficheiro e ações, atividade, definições com teste de ligação. Protegida por token e verificação do Host.
- Testes: 28 automáticos (unitários e de integração contra um servidor WebDAV em memória) e 64 verificações de ponta a ponta num mount FUSE real.
