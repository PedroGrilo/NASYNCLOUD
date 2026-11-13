# CLAUDE.md — contexto para novas sessões

Lê isto antes de mexer no projeto. Detalhes em `docs/ARCHITECTURE.md` (como funciona) e `docs/DECISIONS.md` (porquê).

## O que é

**NASYNCLOUD** (binário, crate e pastas: `nasyncloud`; até à 0.2.1 chamava-se `nasdrive`) é um cliente "Files On-Demand" em Rust para uma NAS com WebDAV. A NAS aparece como uma pasta local; só é descarregado o que se abre; as alterações sobem em segundo plano; funciona offline com o que já está no disco. Linux usa FUSE, macOS um servidor NFS local montado com o `mount_nfs` do sistema (desde a 1.1.0; antes macFUSE), Windows usa a Cloud Files API. Tem uma interface web local (`nasyncloud ui`).

A NAS alvo é uma **UGREEN NASync DH2300** (UGOS). Não tem Docker oficial, por isso nada pode correr na NAS: tudo é cliente, falando com o WebDAV que a UGOS já traz (portas 5005 HTTP / 5006 HTTPS).

## Pessoas e convenções

- Dono: Pedro, engenheiro frontend (React, Vue, TypeScript, sistemas em tempo real). Escreve e espera respostas em **português europeu (pt-PT)**, tom informal. Prefere desenvolvimento modular e iterativo, com resultado pronto a usar.
- Nome: **NASYNCLOUD** em tudo o que o utilizador vê (interface, app, pacotes, documentação); `nasyncloud` em minúsculas nos identificadores técnicos (binários, crates, pastas, xattrs, header). O macOS e o Windows não distinguem maiúsculas em nomes de ficheiros: o `.app` tem `nasyncloud` e `nasyncloud-desktop` lado a lado, nunca `NASYNCLOUD` + `nasyncloud`.
- **README em inglês** (pedido do Pedro), simples, para qualquer pessoa: o que faz, instalar, usar, FAQ. As etiquetas da interface aparecem em pt-PT com a tradução entre parênteses. A parte técnica está em `docs/AVANCADO.md`. A descrição do repositório também é em inglês.
- Idioma no código: código, comentários, mensagens de log e da CLI em **inglês**. Interface (`src/ui/index.html`) e documentação em **pt-PT** ("ficheiro", "pasta", "descarregar", "utilizador", «aspas»).
- Estilo da interface: vidro escuro (glassmorphism), acentos índigo e âmbar, tipografia limpa, grelha responsiva. Sem frameworks nem passo de build.

## Estado atual (v1.3.0, 2026-11-13)

| Parte | Estado |
|---|---|
| Linux (FUSE) | Completo e testado: 28 testes (`cargo test`) + 64 verificações num mount real (`tests/e2e.sh`) |
| macOS (NFS local) | Completo e testado no Mac do Pedro (macOS 27), sem macFUSE: `cargo test` + 63 verificações num mount real (`tests/e2e.sh`; 1 saltada: `st_blocks`). Testados à mão: cópia pelo Finder, `ditto`/`cp -p` com xattrs, ejetar, recuperação depois de `kill -9` |
| Windows (Cloud Files) | Compila (`cargo check --target x86_64-pc-windows-msvc`); **nunca correu** |
| Interface web | Testada pela API no e2e e vista no browser (desktop e telemóvel); capturas atuais em `docs/screenshots/` (`panel.png`, `files.png`), feitas com uma NAS de demonstração (`fake_nas`) e o browser do Claude |
| WebDAV da UGOS | **Nunca testado contra o servidor real.** Testado contra rclone (Go x/net/webdav) e `dav-server` |

Primeiro passo quando houver acesso a Windows ou à NAS real: `nasyncloud -v ui`, configurar numa pasta partilhada de teste e corrigir a partir do registo. A própria UGREEN documenta erros 405 do WebDAV ao criar pastas com rclone/Cyberduck; `WebDav::mkdir` já tenta contornar (stat + barra final), mas é o sítio mais provável de falhar.

## Mapa do código

```
src/
  lib.rs            biblioteca (o que os testes de integração usam)
  config.rs         Config (TOML), password (env → ficheiro → keyring), tamanhos, DEFAULT_UI_PORT
  exclude.rs        Excludes: regras «fica só neste computador» (`.git`, `node_modules`, `*.log`, `build/`, `/caminho`)
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
  ui/panel.html     o painel compacto da app (estado + transferências), servido em /panel
  activity.rs       registo das transferências recentes (junta as do mesmo ficheiro), API /api/transfers
  logging.rs        logger próprio: stderr + últimas 1000 linhas para a interface
  fuse.rs           adaptador FUSE (fuser 0.18)                                 [Linux]
  nfs.rs            servidor NFSv3 em 127.0.0.1 (nfsserve) + mount_nfs; `._*` em memória  [macOS]
  unix.rs           Mounted para Linux/macOS + comandos CLI (xattrs no Linux, API local no macOS)  [unix]
  win.rs            motor Windows (cloud-filter 0.0.7) + Mounted                [windows]
tests/
  common/mod.rs     servidor WebDAV em memória (dav-server) com NAS "em baixo", falhas de PUT, latência
  webdav.rs, vfs.rs testes de integração
  e2e.sh            mount real (FUSE no Linux, NFS no macOS) contra rclone ou fake_nas + API da interface
tools/
  demo.sh           NAS falsa com conteúdo realista + mount + interface em :18731
  screenshot.py     capturas da interface (QtWebEngine), usado para docs/screenshots
desktop/          a app da bandeja / barra de menus (Tauri v2), crate `nasyncloud-desktop` do workspace
  src/main.rs       arranca `nasyncloud serve` (o binário ao lado), ícone, painel junto ao ícone, janela principal
  dist/index.html   página local de erro (o serviço não arrancou)
  icons/            ícones da app e da bandeja (tray-template.png = símbolo monocromático do macOS)
examples/
  fake_nas.rs       NAS falsa (dav-server + Basic auth) para testar sem rclone, p.ex. no macOS
tools/package-macos.sh   gera dist/NASYNCLOUD.app e dist/NASYNCLOUD-<versão>-macos-<arch>.dmg (assinatura ad-hoc)
packaging/macos/  Info.plist (LSUIElement) e LEIA-ME
packaging/windows/LEIA-ME.txt   vai no zip com o nasyncloud.exe
assets/           icon.svg (fonte) e nasyncloud.icns (gerado a partir dele, ver «Lançar uma versão»)
.github/workflows/release.yml   tag v* → testes no macOS e no Windows → .dmg e .exe (zip) → GitHub Release (sem Linux, a pedido do Pedro)
```

`Mounted` tem a mesma API nas duas plataformas (`start`, `stop`, `wait_ended`, `info`, `browse`, `pin`, `free`, `refresh`); `service.rs` só fala com isso.

## Comandos

```sh
cargo build && cargo test
cargo clippy --all-targets                                   # deve dar zero avisos
cargo check --target x86_64-pc-windows-msvc                  # verificação cruzada Windows
cargo check --target aarch64-apple-darwin                    # a partir de outro SO
cargo build && cargo build --example fake_nas && tests/e2e.sh   # Linux (fuse3) ou macOS (nada a instalar); curl, bc, python3; rclone opcional
tools/demo.sh                                                # demo em http://127.0.0.1:18731
```

## Lançar uma versão

1. Subir `version` no `Cargo.toml` (e o `Cargo.lock` com `cargo build`) e mudar «Por lançar» no CHANGELOG para a versão e a data.
2. Commit, depois `git tag vX.Y.Z && git push origin main vX.Y.Z`. A tag tem de ser igual à versão do `Cargo.toml`, senão o workflow falha logo.
3. O `release.yml` corre os testes no macOS e no Windows, gera o `.dmg` e o zip com o `.exe` e publica a Release. Em «Run workflow» (manual) só gera os pacotes, que ficam como artifacts da execução. O Linux não está no workflow: clippy, testes e e2e no Linux só correm à mão. Na última vez que correram no GitHub (Ubuntu 26.04) deram clippy limpo, testes OK e e2e 63/64, e a falha já foi corrigida no teste.
4. As tags publicadas não se movem (o auto mode bloqueia `push -f`). Se uma tag falhar, corrige-se e lança-se a versão seguinte. Foi o que aconteceu à `v0.2.0`, que ficou sem Release.

O ícone vem de `assets/icon.svg`. Para o regenerar no Mac, desenha o SVG num PNG de 1024 px com fundo transparente (o Quick Look/`qlmanage` mete fundo branco, por isso usa um script Swift com `NSImage` + `NSBitmapImageRep`), cria o iconset com `sips -z` (16–512, @1x e @2x) e corre `iconutil -c icns`.

## Atualizações automáticas (tauri-plugin-updater)

- A app lê `https://github.com/PedroGrilo/NASYNCLOUD/releases/latest/download/latest.json` (20 s depois de arrancar, depois a cada 6 h, e no menu «Procurar atualizações»). O `latest.json` é gerado pelo job `release` do workflow, a partir das assinaturas `.sig`.
- **Chave de assinatura**: a pública está em `desktop/tauri.conf.json` (`plugins.updater.pubkey`). A privada e a password estão no Mac do Pedro, em `~/.nasyncloud-updater/` (`nasyncloud.key`, `password`). **Se se perder a privada, as apps instaladas deixam de aceitar atualizações**: guardar uma cópia no gestor de passwords. No GitHub ficam nos segredos `TAURI_SIGNING_PRIVATE_KEY` e `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, que é o próprio Pedro que define, porque o auto mode não deixa escrever segredos. Sem eles, as execuções manuais geram pacotes sem assinatura e as de tag falham logo.
- O que o atualizador descarrega: no macOS, `NASYNCLOUD-macos-arm64.app.tar.gz` (o `.app` inteiro, que ele substitui); no Windows, o instalador NSIS (`tauri.windows.conf.json`, gerado pelo CLI do Tauri, com o `nasyncloud.exe` como `externalBin`).
- Antes de instalar, a app para o serviço (`stop_service`), porque ele corre de dentro da app que vai ser substituída. Depois faz `app.restart()`.
- O painel recebe o estado (`available`, `downloading`, `installing`, `error`) por `eval` em `window.__nasyncloudUpdate`. O «Atualizar agora» é a ação `/app/update`.
- **Testar localmente** (foi feito assim, e funcionou):
  1. Compilar com `TAURI_CONFIG='{"plugins":{"updater":{"endpoints":["http://127.0.0.1:18799/latest.json"],"dangerousInsecureTransportProtocol":true}}}' CARGO_TARGET_DIR=target/updtest cargo build --release -p nasyncloud-desktop`.
  2. Criar um `.app.tar.gz` com `COPYFILE_DISABLE=1 tar --no-mac-metadata` e assiná-lo com `npx @tauri-apps/cli@2.12.4 signer sign -f ~/.nasyncloud-updater/nasyncloud.key -p …`.
  3. Servir um `latest.json` com `python3 -m http.server 18799`.
  4. Arrancar com `NASYNCLOUD_UPDATE_NOW=1`, que instala sem clicar.

## Armadilhas (ler antes de mexer)

- **VFS**: um único `std::sync::Mutex<State>`, **nunca** mantido através de um `.await`; rede sempre fora do lock. Escritas em ficheiros de cache são feitas *dentro* do lock, para não se cruzarem com downloads.
- **Durabilidade**: um ficheiro só passa a `dirty` com o índice gravado com fsync *antes* do primeiro byte alterado (`prepare_write`). Os ficheiros de cache levam fsync antes de o índice afirmar o que têm (`unsynced`). Commits redb sem fsync só ficam permanentes com o próximo commit durável: o `flush(durable)` sabe se ainda deve um (`commits`/`durable_at`). Foi um bug real (cópias de conflito falsas depois de reiniciar).
- **Invalidar cache** cria uma nova geração do ficheiro (`<id>-<gen>`); nunca se altera a antiga, para o índice não mentir depois de um crash.
- **fuser 0.18**: não usar `Session::spawn()`, porque tira o mount ao `SessionUnmounter`. Corre-se `session.run()` numa thread própria; o fim chega por um `watch::Receiver<bool>`.
- **Windows**: os callbacks da Cloud Files correm em threads da própria API e usam `Handle::block_on`. As operações da interface (`browse`, `pin`, `free`) vão por `spawn_blocking`, porque listar uma pasta por abrir dispara um callback. A conexão vive na thread `nasyncloud-sync`.
- **Windows, parar**: o trabalho de fundo (`Engine::work`) corre em fatias (`QUEUE_SLICE`, `POLL_SLICE`) e todas as chamadas à NAS passam por `Engine::net`, que as cancela quando `stop` é ligado. `Mounted::stop` espera no máximo `STOP_WAIT` (20 s). Não voltar a pôr ciclos longos sem olhar para `stopping()`: foi o encravamento em «A parar…».
- **Windows, vigia de pastas**: o `cloud-filter` tem o seu próprio vigia (`ReadDirectoryChangesW`). Quando transborda, varre a pasta inteira com `read_dir`, o que abriria todas as pastas da NAS; por isso o `fetch_placeholders` recusa pedidos do nosso próprio processo, exceto dentro de `Engine::listing` (enumerações nossas de propósito). A recusa mata o vigia; `Engine::run` volta a ligar a pasta (no máximo duas vezes por minuto) e a volta seguinte apanha o que mudou. Qualquer `read_dir` novo de uma pasta que possa não estar aberta tem de ir dentro de `listing`.
- **Windows, apagar cá sem apagar lá**: `as_ours` + `is_ours` (por prefixo: abrange o que está dentro). `prune` só apaga placeholders sincronizados e pastas nunca abertas; o resto fica e volta à fila.
- **Interface**: todo o texto vindo da NAS entra no DOM como texto (função `h()`), nunca `innerHTML`, porque os nomes de ficheiros podem ter qualquer coisa. `[hidden]{display:none!important}` existe porque muitos componentes definem `display`. Em ecrãs estreitos a grelha usa `minmax(0,1fr)`, senão o menu alarga a página.
- **Segurança da interface**: só 127.0.0.1, token no header `x-nasyncloud-token` (guardado em `ui.json`, 0600, mantido entre arranques), verificação do `Host` (421), CSP, corpo até 64 KiB, caminhos com `..` recusados.
- **Testes**: o e2e usa as portas 5005 (rclone) e 18730 (interface); a demo usa 5007 e 18731. Nunca usar `pkill -f` com um padrão que apareça no próprio script: mata a shell que o corre.
- `tests/e2e.sh` começa por matar qualquer `rclone`/`nasyncloud` a correr, incluindo a demo: correr a demo *depois* do e2e.
- `tests/e2e.sh` e `tools/demo.sh` procuram o binário em `target/debug/`; com `CARGO_TARGET_DIR` diferente, passar `BIN=/caminho/nasyncloud`. O e2e aceita `E2E_DIR` (pasta de trabalho), `NAS_SERVER=rclone|fake_nas` e `FAKE_NAS`.
- **macOS, mount NFS**: `mount_nfs` funciona sem root numa pasta do utilizador; `umount`/`unmount(2)` também. É um mount *hard* (sem `soft`): uma leitura lenta da NAS nunca falha por tempo esgotado, mas um serviço morto deixa a pasta pendurada. Por isso, ao arrancar, `nfs::clean_stale` lê a tabela de mounts (`getmntinfo`, sem I/O) e desmonta com `MNT_FORCE` *antes* de qualquer `read_dir`/`canonicalize` da pasta. Nunca fazer `stat` ao mountpoint sem isso.
- **macOS, caches do cliente NFS**: o cliente só larga listagens e conteúdos quando a data muda, e a NAS dá segundos. `Attr::change` (`mut_gen` + `seen_gen`) vai nos nanossegundos. `getattr` usa `getattr_fresh`. `nonegnamecache` e `actimeo=1` são precisos para alterações na NAS aparecerem (o e2e falha sem eles).
- **macOS, `st_blocks`**: o cliente NFS ignora o `used` e calcula-o a partir do tamanho, por isso o `du` não serve no macOS (o e2e salta essa verificação). O FSSTAT do `nfsserve` devolve sempre 1 TiB livre.
- **macOS, `._*` e xattrs**: o NFSv3 não tem xattrs; o macOS grava-os em `._nome`. `nfs.rs` guarda `._*` e `.DS_Store` num `Litter` em memória (ids ≥ 2⁶²): não vão para a NAS, não são listados e perdem-se ao parar. Por isso os xattrs de controlo da CLI não funcionam no macOS e os comandos usam a API local (`/api/status`, `/api/info` só existem no macOS).
- **macOS, abrir/fechar**: o NFS não os tem. `nfs::Handles` abre no `Vfs` no primeiro READ/WRITE e fecha após 1 s (escrita) ou 5 s (leitura) sem uso, nunca com pedidos em curso (`busy`). O `Vfs::create` já devolve o ficheiro aberto para escrita: `use_file(.., already_open=true)`.
- **App (desktop/)**: não depende da crate `nasyncloud` de propósito (o serviço é um processo à parte, e a app continua viva para dizer quando ele falha). Fala com o serviço por HTTP em 127.0.0.1 (token do `ui.json`). As páginas pedem coisas à app navegando para `/app/<ação>` (main, hide, quit, restart, web), que o `on_navigation` interceta: assim não é preciso IPC do Tauri em páginas servidas por outro processo. O painel é uma janela sem moldura, transparente e sempre por cima, que se esconde ao perder o foco; o `hidden_at` evita que o mesmo clique no ícone o volte a abrir.
- **App, compilar para Windows a partir do Mac**: falha no `tauri-build` (precisa de `llvm-rc` para os recursos). Fica para o workflow (runner Windows).
- **Arquivo de atualização no macOS**: criar sempre com `COPYFILE_DISABLE=1 tar --no-mac-metadata`. Sem isso, o tar mete entradas `._*` (AppleDouble) e o atualizador falha a extrair.
- **Porta-chaves e assinatura ad-hoc**: cada build muda a assinatura, por isso, depois de atualizar, o macOS volta a pedir acesso à password. O serviço já abre a interface antes de ler o Porta-chaves e a app espera até 2 minutos por ele. Com Developer ID, o pedido deixa de aparecer.
- **Symlinks** (Linux/macOS): `Kind::Symlink`, alvo em `Record::link`. O WebDAV não os tem, por isso ficam sempre só neste computador (`stays_local`, `local_change`), como os excluídos. Sem isto o Finder falhava a copiar qualquer `node_modules` (`.bin/*`). No Windows não há nada disto.
- **Envios em simultâneo**: `parallel_uploads_auto` (por omissão) dá a cada envio um peso de 1 por MiB (1–32) num orçamento de `MAX_UPLOADS` (64), por ordem: 64 pequenos ou 2 grandes. O motor do Windows envia um de cada vez e ignora isto.
- **macOS, `nfsserve` no repositório** (`vendor/nfsserve`, por `[patch.crates-io]`): a 0.11.0 com a limpeza do `TransactionTracker` limitada a uma vez por segundo (antes, cada pedido varria todos os do último minuto, o que tornava as cópias de muitos ficheiros quadráticas). Ao atualizar o `nfsserve`, ver `vendor/nfsserve/NASYNCLOUD.md`. O `nfsserve` não implementa o COMMIT (responde `FILE_SYNC` a cada WRITE), por isso o `Vfs::fsync` nunca é chamado pelo NFS.
- **macOS, desempenho**: medir sempre com `--release` (o build debug é 5 a 10 vezes mais lento por pedido NFS). Referência: `cp -Rp` do `react-app` (6 684 entradas, `node_modules` excluído) ~30 s; cada ficheiro custa ~20 pedidos NFS (o macOS junta `com.apple.provenance` a cada ficheiro novo, num `._*`). `nfsstat -c` conta os pedidos, mas inclui os de todos os mounts NFS (a app do Pedro faz ~100/s de fundo).
- **zsh**: `log` é um builtin; para os registos do sistema usar `/usr/bin/log show`.
- **App macOS**: `LSUIElement` (sem Dock), por isso a única forma normal de sair é o botão «Sair» da interface (`/api/quit`). Como a assinatura é ad-hoc, o Porta-chaves pode voltar a pedir autorização para a password depois de cada atualização da app.

## Próximos passos possíveis

1. Testar no Windows e contra a UGOS real (ver acima). Correr o e2e no Linux depois das mudanças para o macOS (ver CHANGELOG): o ramo Linux de `fuse.rs` não foi recompilado nem testado.
2. Ícone na bandeja do sistema (crate `tray-icon`) ou um invólucro Tauri à volta da mesma interface.
3. Traduzir para pt-PT as mensagens que aparecem na Atividade (hoje em inglês).
4. Linux/macOS: fila para apagar/renomear offline (hoje exige a NAS).
5. Backend SMB ou SFTP: basta implementar o trait `Remote`.
6. macOS: extensão File Provider (Swift, assinada, precisa da conta Apple Developer) para ter ícones no Finder.
   - macOS: correr o e2e no runner macOS do workflow (já não precisa de instalar nada).
7. Pacotes prontos a instalar: feito (`release.yml`). Falta: CI em cada push/PR (hoje só com tag ou à mão), assinatura e notarização da Apple (Developer ID), ícone no `.exe`, `.dmg` universal (Intel).

## Repositório e máquina local

- GitHub: `https://github.com/PedroGrilo/NASYNCLOUD`, branch `main`.
- Cópia local: `~/ugreen-nas` no Mac do Pedro (Apple Silicon, arm64, macOS 27, SIP ativo). Rust 1.99 e `pkg-config` pelo Homebrew, `gh` autenticado, macFUSE 5.5.0 ainda instalado (já não é usado). **Não há rclone**: o e2e usa o `fake_nas`.
- Verificação Windows no Mac: o `rustup` (`~/.cargo/bin/rustup`) tem o alvo `x86_64-pc-windows-msvc`, mas o `cargo` do PATH é o do Homebrew. Correr `R=~/.cargo/bin/rustup; RUSTC=$($R which rustc) CARGO_TARGET_DIR=target/xwin $($R which cargo) check --target x86_64-pc-windows-msvc`. Não há alvo Linux instalado: o ramo Linux de `fuse.rs` não é compilado no Mac.
- O `tools/demo.sh` continua a ser só para Linux.

## macOS: notas dos testes (2026-11-12)

- No Mac passam `cargo test`, `cargo clippy --all-targets` (zero avisos) e `tests/e2e.sh` (63 passam, 0 falham, 1 saltada), com o mount NFS e sem macFUSE.
- `examples/fake_nas.rs` é a NAS falsa (dav-server sobre uma pasta, Basic auth `pedro`/`segredo`, regista cada pedido em stderr como `GET /caminho bytes=…`): `cargo run --example fake_nas -- <pasta> 127.0.0.1:5015`.
- O binário já não liga ao libfuse (`otool -L target/debug/nasyncloud`).
- Até à 1.0.0 usava-se o macFUSE (motor kext); o motor FSKit do macFUSE nunca montou neste Mac. Ver `docs/DECISIONS.md` (4) para as alternativas consideradas.

## Ambiente de desenvolvimento no sandbox do Claude (claude.ai)

- Não há acesso a `static.rust-lang.org`. O Rust instala-se pelo conda-forge:
  `micromamba create -y -p /opt/rust -c conda-forge rust rust-std-x86_64-pc-windows-msvc rust-std-aarch64-apple-darwin`
  (o micromamba vem de `conda.anaconda.org/conda-forge/linux-64/micromamba-*.tar.bz2`). crates.io funciona.
- O FUSE funciona no sandbox (corre como root): `apt-get install -y fuse3 libssl-dev rclone`.
- Capturas da interface: `apt-get install -y python3-pyqt6.qtwebengine xvfb fonts-inter`, depois `tools/demo.sh` e `xvfb-run -a python3 tools/screenshot.py 18731 <token> /tmp`.
- `cargo build` com 1 CPU demora ~2 min na primeira vez.
- No sandbox usou-se `CARGO_TARGET_DIR=/home/claude/target`, por isso: `BIN=/home/claude/target/debug/nasyncloud tools/demo.sh`.
