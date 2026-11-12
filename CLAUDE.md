# CLAUDE.md — contexto para novas sessões

Lê isto antes de mexer no projeto. Detalhes em `docs/ARCHITECTURE.md` (como funciona) e `docs/DECISIONS.md` (porquê).

## O que é

`nasdrive` é um cliente "Files On-Demand" em Rust para uma NAS com WebDAV. A NAS aparece como uma pasta local; só é descarregado o que se abre; as alterações sobem em segundo plano; funciona offline com o que já está no disco. Linux e macOS usam FUSE, Windows usa a Cloud Files API. Tem uma interface web local (`nasdrive ui`).

A NAS alvo é uma **UGREEN NASync DH2300** (UGOS). Não tem Docker oficial, por isso nada pode correr na NAS: tudo é cliente, falando com o WebDAV que a UGOS já traz (portas 5005 HTTP / 5006 HTTPS).

## Pessoas e convenções

- Dono: Pedro, engenheiro frontend (React, Vue, TypeScript, sistemas em tempo real). Escreve e espera respostas em **português europeu (pt-PT)**, tom informal. Prefere desenvolvimento modular e iterativo, com resultado pronto a usar.
- Idioma no código: código, comentários, mensagens de log e da CLI em **inglês**. Interface (`src/ui/index.html`) e documentação em **pt-PT** ("ficheiro", "pasta", "descarregar", "utilizador", «aspas»).
- Estilo da interface: vidro escuro (glassmorphism), acentos índigo e âmbar, tipografia limpa, grelha responsiva. Sem frameworks nem passo de build.

## Estado atual (v0.1.0, 2026-11-12)

| Parte | Estado |
|---|---|
| Linux (FUSE) | Completo e testado: 28 testes (`cargo test`) + 64 verificações num mount real (`tests/e2e.sh`) |
| macOS (FUSE/macFUSE) | Compila (`cargo check --target aarch64-apple-darwin --features macos-no-mount`); **nunca correu** |
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
  e2e.sh            mount FUSE real contra `rclone serve webdav` + API da interface (Linux)
tools/
  demo.sh           NAS falsa com conteúdo realista + mount + interface em :18731
  screenshot.py     capturas da interface (QtWebEngine), usado para docs/screenshots
```

`Mounted` tem a mesma API nas duas plataformas (`start`, `stop`, `wait_ended`, `info`, `browse`, `pin`, `free`, `refresh`); `service.rs` só fala com isso.

## Comandos

```sh
cargo build && cargo test
cargo clippy --all-targets                                   # deve dar zero avisos
cargo check --target x86_64-pc-windows-msvc                  # verificação cruzada Windows
cargo check --target aarch64-apple-darwin --features macos-no-mount
cargo build && tests/e2e.sh                                  # Linux: fuse3, rclone, curl, bc, python3
tools/demo.sh                                                # demo em http://127.0.0.1:18731
```

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
- `tests/e2e.sh` e `tools/demo.sh` procuram o binário em `target/debug/`; com `CARGO_TARGET_DIR` diferente, passar `BIN=/caminho/nasdrive`.

## Próximos passos possíveis

1. Testar no Windows e contra a UGOS real (ver acima).
2. Ícone na bandeja do sistema (crate `tray-icon`) ou um invólucro Tauri à volta da mesma interface.
3. Traduzir para pt-PT as mensagens que aparecem na Atividade (hoje em inglês).
4. Linux/macOS: fila para apagar/renomear offline (hoje exige a NAS).
5. Backend SMB ou SFTP: basta implementar o trait `Remote`.
6. macOS: extensão File Provider (Swift, assinada) para ter ícones no Finder.
7. CI (GitHub Actions com Linux/macOS/Windows) e binários prontos a instalar.

## Repositório e máquina local

- GitHub: `https://github.com/PedroGrilo/Files-on-Demand-UGOS`, branch `main`.
- Cópia local: `~/ugreen-nas` no Mac do Pedro (Apple Silicon, arm64, macOS 27). Rust 1.99 e `pkg-config` pelo Homebrew, `gh` autenticado, **macFUSE já instalado**.
- Isto quer dizer que o macOS pode finalmente ser testado a sério aqui (até agora só compilava): `cargo build && cargo test`, depois `nasdrive -v ui` numa pasta de teste. O `tests/e2e.sh` e o `tools/demo.sh` são só Linux (usam `fusermount`/`rclone` como no sandbox).

## Ambiente de desenvolvimento no sandbox do Claude (claude.ai)

- Não há acesso a `static.rust-lang.org`. O Rust instala-se pelo conda-forge:
  `micromamba create -y -p /opt/rust -c conda-forge rust rust-std-x86_64-pc-windows-msvc rust-std-aarch64-apple-darwin`
  (o micromamba vem de `conda.anaconda.org/conda-forge/linux-64/micromamba-*.tar.bz2`). crates.io funciona.
- O FUSE funciona no sandbox (corre como root): `apt-get install -y fuse3 libssl-dev rclone`.
- Capturas da interface: `apt-get install -y python3-pyqt6.qtwebengine xvfb fonts-inter`, depois `tools/demo.sh` e `xvfb-run -a python3 tools/screenshot.py 18731 <token> /tmp`.
- `cargo build` com 1 CPU demora ~2 min na primeira vez.
- No sandbox usou-se `CARGO_TARGET_DIR=/home/claude/target`, por isso: `BIN=/home/claude/target/debug/nasdrive tools/demo.sh`.
