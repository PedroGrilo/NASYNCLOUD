# Changelog

## 1.0.0 — 2026-11-12

- **Atualizações automáticas** na app: verifica as Releases (`latest.json`) ao arrancar e a cada 6 horas, mostra uma notificação e um aviso no painel, e «Atualizar agora» descarrega, confirma a assinatura, instala e reinicia. O menu do ícone tem «Procurar atualizações».
- **Windows: instalador** (`NASYNCLOUD-…-windows-x64-setup.exe`, por utilizador, sem administrador) em vez do zip.
- O repositório passou a ser público.
- Correções: o serviço abre a interface antes de ler o Porta-chaves (que pode ficar à espera de autorização); «Tentar de novo» no painel voltava a mostrar o painel sem arrancar o serviço.
- **Novo nome: NASYNCLOUD** (antes nasdrive), com logótipo novo: uma nuvem com setas de sincronização. Binário `nasyncloud`, app `NASYNCLOUD.app`, repositório `PedroGrilo/NASYNCLOUD`. As definições, a cache e a password do Porta-chaves da versão anterior passam para o novo nome sozinhas, na primeira vez que corre.
- **App de barra de menus / área de notificação** (`desktop/`, Tauri v2), como o OneDrive: o ícone abre um painel com o estado, as transferências em curso (com progresso) e as recentes, o espaço usado e um menu (abrir a interface completa numa janela, Definições, pausar, sair). Arranca e para sozinha o serviço. No macOS vai no `.dmg` em vez da app anterior. No Windows, o zip leva o `nasyncloud-desktop.exe` e o `nasyncloud.exe`.
- Motor: regista as transferências (`/api/transfers`), juntando as do mesmo ficheiro.
- `nasyncloud serve`: como o `ui`, mas sem abrir o browser (é o que a app usa).

## 0.2.1 — 2026-11-12

Primeira versão com pacotes. A tag `v0.2.0` ficou sem Release porque dois testes falhavam no Windows; foram corrigidos nesta versão.

- Testes: passam no Windows (pela primeira vez) e no kernel Linux recente.
- **macOS funciona e está testado** (macOS 27, Apple Silicon, macFUSE 5.5, motor kext): 28 testes e 63 verificações num mount real.
- macOS: o mount passa a ser feito com `fuse_mount` + `Session::from_fd`, porque o `fuse_mount_compat25` usado pelo `fuser` 0.18 falha sempre no macFUSE 5.
- macOS: alterações feitas na NAS por outros passam a aparecer. `getattr`/`open` releem a pasta quando passou o `dir_ttl` (o macFUSE não repete o `lookup`), e o `open` pede ao kernel que largue as páginas em cache (`FOPEN_PURGE_UBC`).
- `tests/e2e.sh` corre em Linux e macOS, com o rclone ou com a NAS falsa `examples/fake_nas.rs` (dav-server), e devolve erro se alguma verificação falhar.
- Pacotes prontos a instalar: `.dmg` para macOS (`NASYNCLOUD.app`, que corre em segundo plano, com ícone; avisa se faltar o macFUSE), `.zip` com o `nasyncloud.exe` para Windows. Gerados pelo GitHub Actions (`release.yml`) a partir de uma tag `v*`, que também publica a Release. `tools/package-macos.sh` gera o `.dmg` localmente.
- Interface: botão «Sair», que desmonta a pasta e encerra o NASYNCLOUD depois de terminar os envios pendentes.

## 0.1.0 — 2026-11-12

- Cliente WebDAV (PROPFIND, GET por intervalos, PUT, MKCOL, MOVE, DELETE, quota), com contornos para servidores que ignoram `Range` ou respondem 405 a MKCOL.
- Motor Linux/macOS (FUSE): ficheiros a pedido com cache por intervalos, manter offline, libertar espaço, limite de cache com LRU, uploads em segundo plano, trabalho offline com fila persistente, conflitos com cópia, índice com recuperação após crash.
- Motor Windows (Cloud Files API): placeholders, hidratação a pedido, envio de alterações locais, espelho de apagar/renomear, sondagem da NAS. Compila; ainda não testado.
- Linha de comandos: `ui`, `setup`, `mount`, `pin`, `unpin`, `free`, `status`, `ls`, `refresh`, `unmount`, `unregister`.
- Interface web local (`nasyncloud ui`): estado, explorador com estado por ficheiro e ações, atividade, definições com teste de ligação. Protegida por token e verificação do Host.
- Testes: 28 automáticos (unitários e de integração contra um servidor WebDAV em memória) e 64 verificações de ponta a ponta num mount FUSE real.
