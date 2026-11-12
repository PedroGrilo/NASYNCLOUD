# CLAUDE.md — contexto para novas sessões

Lê isto antes de mexer no projeto. Detalhes em `docs/ARCHITECTURE.md` (como funciona) e `docs/DECISIONS.md` (porquê).

## O que é

`nasdrive` é um cliente "Files On-Demand" em Rust para uma NAS com WebDAV. A NAS aparece como uma pasta local; só é descarregado o que se abre; as alterações sobem em segundo plano; funciona offline com o que já está no disco. Linux e macOS usam FUSE, Windows usa a Cloud Files API. Tem uma interface web local (`nasdrive ui`).

A NAS alvo é uma **UGREEN NASync DH2300** (UGOS). Não tem Docker oficial, por isso nada pode correr na NAS: tudo é cliente, falando com o WebDAV que a UGOS já traz (portas 5005 HTTP / 5006 HTTPS).

## Pessoas e convenções

- Dono: Pedro, engenheiro frontend (React, Vue, TypeScript, sistemas em tempo real). Escreve e espera respostas em **português europeu (pt-PT)**, tom informal. Prefere desenvolvimento modular e iterativo, com resultado pronto a usar.
- Idioma no código: código, comentários, mensagens de log e da CLI em **inglês**. Interface (`src/ui/index.html`) e documentação em **pt-PT** ("ficheiro", "pasta", "descarregar", "utilizador", «aspas»).
- Estilo da interface: vidro escuro (glassmorphism), acentos índigo e âmbar, tipografia limpa, grelha responsiva. Sem frameworks nem passo de build.

## Estado atual (v0.2.0, 2026-11-12)

| Parte | Estado |
|---|---|
| Linux (FUSE) | Completo e testado: 28 testes (`cargo test`) + 64 verificações num mount real (`tests/e2e.sh`) |
| macOS (FUSE/macFUSE) | Completo e testado no Mac do Pedro (macOS 27, macFUSE 5.5, motor kext): 28 testes + 63 verificações num mount real (`tests/e2e.sh`; 1 saltada: `st_blocks`) |
| Windows (Cloud Files) | Compila (`cargo check --target x86_64-pc-windows-msvc`); **nunca correu** |
| Interface web | Testada pela API no e2e e verificada visualmente (QtWebEngine em Xvfb), em desktop e largura de telemóvel; capturas em `docs/screenshots/` |
| WebDAV da UGOS | **Nunca testado contra o servidor real.** Testado contra rclone (Go x/net/webdav) e `dav-server` |

Primeiro passo quando houver acesso a Windows ou à NAS real: `nasdrive -v ui`, configurar numa pasta partilhada de teste e corrigir a partir do registo. A própria UGREEN documenta erros 405 do WebDAV ao criar pastas com rclone/Cyberduck; `WebDav::mkdir` já tenta contornar (stat + barra final), mas é o sítio mais provável de falhar.

## Mapa do código

```
src/
  lib.rs            biblioteca (o que os testes de integração usam)
  config.rs         Config (TOML), password (env → ficheiro → keyring), tamanhos, DEFAULT_UI_PORT
  remote.rs         trait Remote (backend), Version/Entry/RemoteError, conflict_name(), timestamp()
  webdav.rs         implementação WebDAV (reqwest): PROPFIND, GET com Range, PUT, MKCOL, MOVE, DELETE
  ranges.rs         RangeSet: intervalos de bytes presentes na cache          [unix]
  store.rs          índice persistente em redb (um Record JSON por nó)         [unix]
  vfs/mod.rs        motor Linux/macOS: árvore, cache, operações de ficheiros   [unix]
  vfs/transfer.rs   downloads por intervalos, uploads em fila, conflitos       [unix]
  vfs/sync.rs       listagens e merge, pastas fixadas, deteção offline, LRU    [unix]
  main.rs           CLI (clap), `setup` interativo, connect()
  service.rs        serviço: config + ciclo de vida do mount + run() (usado por `mount` e `ui`)
  ui.rs             servidor HTTP local (hyper) + API JSON + abrir browser
  ui/index.html     a interface inteira (HTML+CSS+JS), embutida com include_str!
  logging.rs        logger próprio: stderr + últimas 1000 linhas para a interface
  fuse.rs           adaptador FUSE (fuser 0.18)                                 [unix]
  unix.rs           Mounted para Linux/macOS + comandos CLI via xattrs          [unix]
  win.rs            motor Windows (cloud-filter 0.0.7) + Mounted                [windows]
tests/
  common/mod.rs     servidor WebDAV em memória (dav-server) com NAS "em baixo", falhas de PUT, latência
  webdav.rs, vfs.rs testes de integração
  e2e.sh            mount FUSE real contra rclone ou fake_nas + API da interface (Linux e macOS)
tools/
  demo.sh           NAS falsa com conteúdo realista + mount + interface em :18731
  screenshot.py     capturas da interface (QtWebEngine), usado para docs/screenshots
examples/
  fake_nas.rs       NAS falsa (dav-server + Basic auth) para testar sem rclone, p.ex. no macOS
tools/package-macos.sh   gera dist/nasdrive.app e dist/nasdrive-<versão>-macos-<arch>.dmg (assinatura ad-hoc)
packaging/macos/  Info.plist (LSUIElement), lançador nasdrive-app (verifica o macFUSE, regista em ~/Library/Logs), LEIA-ME
packaging/windows/LEIA-ME.txt   vai no zip com o nasdrive.exe
assets/           icon.svg (fonte) e nasdrive.icns (gerado a partir dele, ver «Lançar uma versão»)
.github/workflows/release.yml   tag v* → testes + e2e Linux → .dmg, .exe (zip), binário Linux → GitHub Release
```

`Mounted` tem a mesma API nas duas plataformas (`start`, `stop`, `wait_ended`, `info`, `browse`, `pin`, `free`, `refresh`); `service.rs` só fala com isso.

## Comandos

```sh
cargo build && cargo test
cargo clippy --all-targets                                   # deve dar zero avisos
cargo check --target x86_64-pc-windows-msvc                  # verificação cruzada Windows
cargo check --target aarch64-apple-darwin --features macos-no-mount
cargo build && cargo build --example fake_nas && tests/e2e.sh   # Linux (fuse3) ou macOS (macFUSE); curl, bc, python3; rclone opcional
tools/demo.sh                                                # demo em http://127.0.0.1:18731
```

## Lançar uma versão

1. Subir `version` no `Cargo.toml` (e o `Cargo.lock` com `cargo build`) e mudar «Por lançar» no CHANGELOG para a versão e a data.
2. Commit, depois `git tag vX.Y.Z && git push origin main vX.Y.Z`. A tag tem de ser igual à versão do `Cargo.toml`, senão o workflow falha logo.
3. O `release.yml` corre clippy, testes e e2e no Linux e testes no macOS e no Windows. Depois gera os três pacotes e publica a Release. Em «Run workflow» (manual) só gera os pacotes, que ficam como artifacts da execução.

O ícone vem de `assets/icon.svg`. Para o regenerar no Mac, desenha o SVG num PNG de 1024 px com fundo transparente (o Quick Look/`qlmanage` mete fundo branco, por isso usa um script Swift com `NSImage` + `NSBitmapImageRep`), cria o iconset com `sips -z` (16–512, @1x e @2x) e corre `iconutil -c icns`.

## Armadilhas (ler antes de mexer)

- **VFS**: um único `std::sync::Mutex<State>`, **nunca** mantido através de um `.await`; rede sempre fora do lock. Escritas em ficheiros de cache são feitas *dentro* do lock, para não se cruzarem com downloads.
- **Durabilidade**: um ficheiro só passa a `dirty` com o índice gravado com fsync *antes* do primeiro byte alterado (`prepare_write`). Os ficheiros de cache levam fsync antes de o índice afirmar o que têm (`unsynced`). Commits redb sem fsync só ficam permanentes com o próximo commit durável: o `flush(durable)` sabe se ainda deve um (`commits`/`durable_at`). Foi um bug real (cópias de conflito falsas depois de reiniciar).
- **Invalidar cache** cria uma nova geração do ficheiro (`<id>-<gen>`); nunca se altera a antiga, para o índice não mentir depois de um crash.
- **fuser 0.18**: não usar `Session::spawn()`, porque tira o mount ao `SessionUnmounter`. Corre-se `session.run()` numa thread própria; o fim chega por um `watch::Receiver<bool>`.
- **Windows**: os callbacks da Cloud Files correm em threads da própria API e usam `Handle::block_on`. As operações da interface (`browse`, `pin`, `free`) vão por `spawn_blocking`, porque listar uma pasta por abrir dispara um callback. A conexão vive na thread `nasdrive-sync`.
- **Interface**: todo o texto vindo da NAS entra no DOM como texto (função `h()`), nunca `innerHTML`, porque os nomes de ficheiros podem ter qualquer coisa. `[hidden]{display:none!important}` existe porque muitos componentes definem `display`. Em ecrãs estreitos a grelha usa `minmax(0,1fr)`, senão o menu alarga a página.
- **Segurança da interface**: só 127.0.0.1, token no header `x-nasdrive-token` (guardado em `ui.json`, 0600, mantido entre arranques), verificação do `Host` (421), CSP, corpo até 64 KiB, caminhos com `..` recusados.
- **Testes**: o e2e usa as portas 5005 (rclone) e 18730 (interface); a demo usa 5007 e 18731. Nunca usar `pkill -f` com um padrão que apareça no próprio script: mata a shell que o corre.
- `tests/e2e.sh` começa por matar qualquer `rclone`/`nasdrive` a correr, incluindo a demo: correr a demo *depois* do e2e.
- `tests/e2e.sh` e `tools/demo.sh` procuram o binário em `target/debug/`; com `CARGO_TARGET_DIR` diferente, passar `BIN=/caminho/nasdrive`. O e2e aceita `E2E_DIR` (pasta de trabalho), `NAS_SERVER=rclone|fake_nas` e `FAKE_NAS`.
- **macOS, mount**: o `fuse_mount_compat25` que o `fuser` 0.18 usa falha sempre no macFUSE 5 (-1, errno 0). `fuse.rs` monta com `fuse_mount` + `Session::from_fd` (módulo `macfuse`) e desmonta com `unmount(2)`. Não voltar a `Session::new` no macOS.
- **macOS, alterações remotas**: o macFUSE não repete `lookup` quando o nome expira. Por isso `getattr`/`open` usam `Vfs::getattr_fresh`, que relê a pasta-mãe, e o `open` devolve `FOPEN_PURGE_UBC`. Sem isto, um ficheiro alterado na NAS fica com o conteúdo antigo.
- **macOS, `st_blocks`**: o macFUSE ignora-o e calcula-o a partir do tamanho, por isso o `du` não serve no macOS (o e2e salta essa verificação).
- **zsh**: `log` é um builtin; para os registos do sistema usar `/usr/bin/log show`.
- **macOS, aviso «fuse: forking a threaded process is unsafe»**: vem do `fuse_mount` do libfuse, que lança o `mount_macfuse` a partir de um processo com threads. É inofensivo (o `malloc` do macOS tem handlers de `fork`), mas convém não pôr trabalho entre o `fork` e o `exec`.
- **App macOS**: `LSUIElement` (sem Dock), por isso a única forma normal de sair é o botão «Sair» da interface (`/api/quit`). Como a assinatura é ad-hoc, o Porta-chaves pode voltar a pedir autorização para a password depois de cada atualização da app.

## Próximos passos possíveis

1. Testar no Windows e contra a UGOS real (ver acima). Correr o e2e no Linux depois das mudanças para o macOS (ver CHANGELOG): o ramo Linux de `fuse.rs` não foi recompilado nem testado.
2. Ícone na bandeja do sistema (crate `tray-icon`) ou um invólucro Tauri à volta da mesma interface.
3. Traduzir para pt-PT as mensagens que aparecem na Atividade (hoje em inglês).
4. Linux/macOS: fila para apagar/renomear offline (hoje exige a NAS).
5. Backend SMB ou SFTP: basta implementar o trait `Remote`.
6. macOS: extensão File Provider (Swift, assinada) para ter ícones no Finder.
   - macOS: aceitar e ignorar os xattrs `com.apple.*` em vez de os recusar (tira o aviso do `cp`; confirmar o comportamento do Finder).
7. Pacotes prontos a instalar: feito (`release.yml`). Falta: CI em cada push/PR (hoje só com tag ou à mão), assinatura e notarização da Apple (Developer ID), ícone no `.exe`, `.dmg` universal (Intel).

## Repositório e máquina local

- GitHub: `https://github.com/PedroGrilo/Files-on-Demand-UGOS`, branch `main`.
- Cópia local: `~/ugreen-nas` no Mac do Pedro (Apple Silicon, arm64, macOS 27, SIP ativo). Rust 1.99 e `pkg-config` pelo Homebrew, `gh` autenticado, macFUSE 5.5.0 com o kext autorizado. **Não há rclone**: o e2e usa o `fake_nas`.
- Verificação Windows no Mac: o `rustup` (`~/.cargo/bin/rustup`) tem o alvo `x86_64-pc-windows-msvc`, mas o `cargo` do PATH é o do Homebrew. Correr `R=~/.cargo/bin/rustup; RUSTC=$($R which rustc) CARGO_TARGET_DIR=target/xwin $($R which cargo) check --target x86_64-pc-windows-msvc`. Não há alvo Linux instalado: o ramo Linux de `fuse.rs` não é compilado no Mac.
- O `tools/demo.sh` continua a ser só para Linux.

## macOS: notas dos testes (2026-11-12)

- No Mac passam `cargo test` (28/28), `cargo clippy --all-targets` (zero avisos) e `tests/e2e.sh` (63 passam, 0 falham, 1 saltada).
- `examples/fake_nas.rs` é a NAS falsa (dav-server sobre uma pasta, Basic auth `pedro`/`segredo`, regista cada pedido em stderr como `GET /caminho bytes=…`): `cargo run --example fake_nas -- <pasta> 127.0.0.1:5015`.
- Motor **kext** do macFUSE: funciona. Antes de o Pedro o autorizar e reiniciar, dava `mount_macfuse: the file system is not available (2)`.
- Motor **FSKit** (`NASDRIVE_MACFUSE_BACKEND=fskit`): não monta. O módulo `io.macfuse.app.fsmodule.macfuse` não aparece em `pluginkit -m -p com.apple.fskit.fsmodule`, e nem um «hello» em C com libfuse3 monta com `-o backend=fskit`. O problema é do sistema, não do nasdrive.
- O `cp` avisa «could not copy extended attributes … Operation not permitted» quando a origem tem `com.apple.*` (por exemplo `com.apple.provenance`). É o `noapplexattr`: o kernel recusa com EPERM. O conteúdo chega bem.

## Ambiente de desenvolvimento no sandbox do Claude (claude.ai)

- Não há acesso a `static.rust-lang.org`. O Rust instala-se pelo conda-forge:
  `micromamba create -y -p /opt/rust -c conda-forge rust rust-std-x86_64-pc-windows-msvc rust-std-aarch64-apple-darwin`
  (o micromamba vem de `conda.anaconda.org/conda-forge/linux-64/micromamba-*.tar.bz2`). crates.io funciona.
- O FUSE funciona no sandbox (corre como root): `apt-get install -y fuse3 libssl-dev rclone`.
- Capturas da interface: `apt-get install -y python3-pyqt6.qtwebengine xvfb fonts-inter`, depois `tools/demo.sh` e `xvfb-run -a python3 tools/screenshot.py 18731 <token> /tmp`.
- `cargo build` com 1 CPU demora ~2 min na primeira vez.
- No sandbox usou-se `CARGO_TARGET_DIR=/home/claude/target`, por isso: `BIN=/home/claude/target/debug/nasdrive tools/demo.sh`.
