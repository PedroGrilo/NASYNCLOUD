# Arquitetura

## Visão geral

```
            browser ──HTTP 127.0.0.1──▶ ui.rs ──┐
                                                 ▼
  nasyncloud mount / ui ──▶ service.rs ──▶ Mounted (unix.rs | win.rs)
                                                 │
                   Linux:   fuse.rs ◀── vfs/ (árvore, cache, índice redb)
                   macOS:   mount_nfs ──▶ nfs.rs (NFS em 127.0.0.1) ◀── vfs/
                   Windows: Cloud Files API ◀── win.rs (placeholders no NTFS)
                                                 │
                                       trait Remote ──▶ webdav.rs ──▶ NAS
```

`service.rs` é o processo de longa duração: tem as definições, o mount (`Arc<Mounted>`), a fase (`stopped`/`starting`/`running`/`stopping`) e o último erro. Arranque, paragem e reconfiguração são serializados por um `tokio::sync::Mutex`. Um vigia por mount deteta quando foi desmontado por fora; em modo `mount` (sem browser) o processo termina, em modo `ui` fica à espera.

## Backend (`remote.rs`, `webdav.rs`)

O trait `Remote` tem oito operações: `list`, `stat`, `open_range`, `upload`, `mkdir`, `delete`, `rename`, `quota`. Os caminhos são relativos à raiz configurada, separados por `/`, sem barra inicial.

`Version { etag, mtime, size }` identifica uma revisão. Duas versões são "a mesma" se o tamanho bate e o ETag (sem `W/` nem aspas) bate, ou o mtime quando não há ETag.

WebDAV: PROPFIND com `Depth: 1`, interpretado só pelo nome local dos elementos (os prefixos variam entre servidores). Os redirecionamentos não são seguidos, porque transformariam um PROPFIND num GET. Downloads com `Range`; um servidor que ignore o `Range` é tratado saltando bytes. Uploads com `Content-Length` (nunca chunked), em stream a partir do ficheiro local, com vigia de paragem (120 s sem progresso). `mkdir` com contorno para servidores que respondem 405. Os erros são mapeados para `RemoteError`, e `is_offline()` decide se se trata de "a NAS não responde".

## Motor Linux/macOS (`vfs/`)

### Modelo

Árvore de nós em memória (`HashMap<NodeId, Node>`), carregada à medida que as pastas são listadas. Cada nó tem uma parte persistida (`store::Record`: nome, pai, tamanho, mtime, versão remota, fixado, intervalos em cache, geração da cache, `dirty`, atime, `listed`) e uma parte só em memória (filhos, aberturas, downloads e upload em curso, erro).

Um único `Mutex<State>` protege tudo e **nunca** é mantido através de um `.await`.

### Cache

Os dados de cada ficheiro vivem num ficheiro esparso `data/<id & 0xff>/<id>-<geração>`. Um `RangeSet` regista que bytes estão presentes. Invalidar a cache (o ficheiro mudou na NAS, libertar espaço, truncar) cria uma nova geração; o ficheiro antigo só é apagado, nunca alterado.

### Índice e recuperação

O índice é um ficheiro redb com uma tabela `nodes` (`u64` → `Record` em JSON, com defaults serde para poder evoluir) e uma `meta` (`next_id`). O `flusher` grava sem fsync à medida que as coisas acontecem e com fsync a cada 5 s e ao sair. Os ficheiros de cache levam fsync antes de o índice afirmar o que têm.

Ao arrancar, a árvore é reconstruída e os nós órfãos são descartados. Depois reconcilia-se com o disco: para ficheiros `dirty`, o ficheiro de cache é a verdade; para os outros, intervalos sem ficheiro são esquecidos. Ficheiros de cache sem dono são apagados e tudo o que ainda tem de subir volta à fila.

### Leituras

`read()` serve logo o que está em cache. Para o resto, `demand()` reaproveita um stream que já vá a caminho (até 2 MiB à frente) ou abre um novo, com até 4 por ficheiro. Cada stream escreve na cache e avisa quem espera por um canal `watch`. Há contrapressão: o stream fica no máximo `readahead` à frente do leitor e fecha ao fim de 5 s sem procura. Se o ETag ou o tamanho já não batem com o esperado, o stream falha com `Changed` e a pasta é relida.

### Escritas

Antes de alterar um ficheiro no sítio, ele tem de estar todo em cache e marcado `dirty` com o índice já gravado com fsync (`prepare_write`). Truncar a 0 dispensa o download. Ao fechar o último escritor, o ficheiro entra na fila de upload com `upload_delay`.

### Uploads (`transfer.rs`)

A fila (`jobs`) tem momento de execução e tentativas, com backoff de 2^n s até 300 s. Há um limite de paralelismo e as pastas são criadas primeiro (ids menores primeiro).

Antes de cada upload é feito um `stat` remoto. Se a versão na NAS não for a versão-base local, cria-se uma cópia de conflito: o ficheiro local muda de nome e a versão da NAS reaparece. O par fica em `conflicts.rs` (ver «Conflitos»). Caso contrário, o upload vai para `.<pid>-<ns>-<seq>.nasyncloud-part` e depois MOVE com overwrite. Se o servidor recusar o MOVE, passa a upload direto (e marca `overwrite_ok` quando um upload direto interrompido pode ter deixado meia cópia nossa). No fim, verifica o tamanho e adota a versão nova.

### Conflitos (`conflicts.rs`)

Resolver um conflito nunca bloqueia nada: a cópia de conflito é feita logo e enviada como outro ficheiro qualquer. `Conflicts` guarda os pares (`path` original, `copy`, a versão local e a versão da NAS vista nesse momento) em `conflicts.json`, no `state_dir`, escrito por cima de um temporário. Os ids vêm do relógio (µs), para a app nunca notificar duas vezes o mesmo. Ao listar, sai da lista o que já não tem cópia (apagada ou renomeada à mão).

- `mine`: um `stat` à NAS; se a versão lá não for a guardada (`Version::same`), não se faz nada, guarda-se a nova e devolve-se `changed` (escolher outra vez substitui essa). Linux/macOS: `refresh` da pasta e `Vfs::rename(copy → original)`, que faz MOVE com overwrite, ou, se a cópia ainda não subiu, herda a versão da NAS como base e sobe por cima dela. Windows: só MOVE na NAS (exige a cópia já sincronizada, senão `sending`), e o `poll_dir` seguinte atualiza o placeholder original e remove o da cópia.
- `nas`: se a NAS já não tiver o ficheiro, `nas_gone`. Senão apaga a cópia (Linux/macOS: `Vfs::unlink`; Windows: DELETE na NAS e `remove_ours`).
- `both`: só sai da lista.

### Sincronização (`sync.rs`)

As listagens valem `dir_ttl`. Se a NAS estiver lenta, serve-se a listagem antiga ao fim de 4 s. O `merge_listing` só aplica se a pasta não mudou localmente entretanto (`mut_gen`). Regras do merge:

- Mudou na NAS e não há alterações locais: a cache é descartada.
- Apagado na NAS mas editado aqui: as edições ganham e voltam a subir.
- Ficheiro que passou a pasta ou vice-versa: substituído, se não houver alterações locais.

Tarefas de fundo:

- `flusher`: grava o índice.
- `uploader`: processa a fila de uploads.
- `pinner`: mantém completas as pastas fixadas, a cada `poll_interval`.
- `prober`: deteta o regresso da NAS (a cada 10 s quando offline) e a quota (a cada 5 min).
- `evictor`: LRU por atime até 90% de `cache_max`; ignora fixados, modificados, abertos e em transferência.

### FUSE (`fuse.rs`, Linux)

Cada pedido do kernel é passado ao runtime Tokio. Os atributos são guardados em cache 1 s. `blocks` reflete os bytes realmente locais (`du`). Os comandos da CLI (`pin`, `free`, `status`...) falam com o mount por atributos estendidos `user.nasyncloud.*`, que não aparecem em `listxattr` para o `cp -a` não os copiar.

`getattr` e `open` usam `Vfs::getattr_fresh`, que primeiro relê a pasta-mãe se a listagem tiver mais de `dir_ttl`. No Linux isto já acontecia pelo `lookup`, que o kernel repete quando o nome expira; o NFS do macOS precisa disto.

### NFS (`nfs.rs`, macOS)

Desde a 1.1.0, o macOS não usa FUSE. O serviço abre um servidor NFSv3 (crate `nfsserve`) em 127.0.0.1, numa porta livre, e monta-o com o `mount_nfs` do sistema, que um utilizador normal pode usar numa pasta sua. Não há macFUSE, extensão de sistema nem password de administrador.

- **Opções do mount**: `vers=3,tcp,port=P,mountport=P,locallocks,intr,actimeo=1,nonegnamecache,rsize=1048576,wsize=1048576,nfc`. É um mount *hard*: uma leitura lenta da NAS nunca dá erro por tempo esgotado. `locallocks` faz os bloqueios de ficheiros funcionarem (o Office precisa deles), `nonegnamecache` faz um ficheiro acabado de chegar à NAS aparecer logo, e `nfc` manda os nomes na forma que uma NAS Linux usa.
- **Abrir e fechar**: o NFS não tem `open`/`close`. O primeiro READ ou WRITE abre o ficheiro no `Vfs` (`open_file`) e o ficheiro é fechado (`release`) depois de 1 s sem escritas ou 5 s sem leituras. Fechar um ficheiro alterado é o que agenda o upload, como no FUSE. Um contador de pedidos em curso impede que um ficheiro seja fechado a meio de um pedido (por exemplo, um WRITE à espera de descarregar o ficheiro todo).
- **Datas**: a NAS dá segundos inteiros, e o cliente só larga a listagem ou o conteúdo em cache quando a data muda. Os nanossegundos levam `Attr::change` (`mut_gen` + `seen_gen`), que sobe com cada alteração local da pasta e com cada listagem da NAS que muda o nó. `getattr` usa `getattr_fresh`.
- **`._*` e `.DS_Store`**: o NFSv3 não tem atributos estendidos, por isso o macOS guarda-os em ficheiros AppleDouble `._nome`. Esses ficheiros e os `.DS_Store` ficam num `Litter` em memória (ids a partir de 2⁶²), nunca vão para a NAS e não aparecem nas listagens. Os xattrs funcionam enquanto o serviço corre e perdem-se quando ele para.
- **Erros**: offline dá `NFS3ERR_IO` (EIO), porque o NFS não tem "host is down".
- **Fim do mount**: uma tarefa lê a tabela de mounts (`getmntinfo`, sem perguntar nada ao mount) a cada 0,5 s. Se o mount desaparecer (ejetado no Finder), fecha os ficheiros, para o servidor e avisa o serviço. Ao arrancar, um mount nosso deixado por um serviço morto é desmontado com `MNT_FORCE` *antes* de se olhar para a pasta, porque olhar para um mount hard sem servidor fica à espera.
- **CLI**: os xattrs de controlo não chegam ao servidor, por isso no macOS os comandos usam a API local (`/api/status`, `/api/info`, `/api/pin`, `/api/free`, `/api/refresh`) com o token do `ui.json`.
- **Limites**: o macOS calcula `st_blocks` a partir do tamanho (ignora o `used`), e o FSSTAT do `nfsserve` devolve sempre 1 TiB livre.

## App de bandeja (`desktop/`)

Uma casca Tauri v2 que não tem o motor: arranca o binário `nasyncloud` que está ao lado (`nasyncloud serve`, o modo `Background` do serviço: interface sem browser) e mostra as páginas que ele serve em 127.0.0.1. São duas: `/panel`, o painel compacto junto ao ícone, e `/`, a interface completa numa janela normal. Lê o porto e o token do `ui.json`, e para saber se o serviço está vivo usa um GET `/api/ping` em HTTP simples, sobre `TcpStream`.

As páginas pedem coisas à app navegando para `/app/<ação>`, e a app interceta essas navegações em `on_navigation`. Se o serviço não arrancar, o painel mostra uma página local (`desktop/dist/index.html`) com o motivo, que recebe por `initialization_script`. O painel lê `/api/state`, `/api/transfers` e `/api/conflicts` a cada 1,5 s enquanto está visível. A app lê `/api/conflicts` a cada 10 s (GET com corpo, no mesmo `TcpStream`) e notifica cada conflito novo uma vez (`watch_conflicts`). As transferências vêm de `activity.rs`: o motor chama `begin`/`progress`/`end` nos downloads (`run_download`) e nos uploads (`upload_file`, com uma guarda RAII), e as transferências do mesmo ficheiro com menos de 2 minutos de intervalo juntam-se numa só entrada.

## Motor Windows (`win.rs`)

O próprio NTFS guarda a árvore, em placeholders. A identidade de cada placeholder é a `Version` em JSON. O registo da sync root é feito por (endereço, utilizador), com hidratação `Full`, população `Full`, desidratação automática permitida e fixar permitido.

Callbacks:

- `fetch_placeholders`: lista a pasta.
- `fetch_data`: GET com Range, em blocos de 4 KiB alinhados (≥256 KiB por escrita) e com progresso.
- `delete` / `rename`: são feitos primeiro na NAS e só depois confirmados ao Windows.
- `dehydrate`: aceite sempre.
- `state_changed`: o vigia da pasta põe caminhos na fila.

A thread `nasyncloud-sync` tem a ligação e o ciclo de trabalho. Processa a fila depois de 2 s de acalmia: envia ficheiros novos ou modificados e converte-os em placeholders, e trata de fixar/desafixar. Sonda as pastas já abertas a cada `poll_interval`, ou a cada 15 s quando está offline. As operações feitas pelo próprio motor ficam marcadas em `ours`, para não voltarem como eco.

O Windows só pede o conteúdo de uma pasta (`fetch_placeholders`) quando é outro programa a abri-la. Quando é o serviço que precisa dela (o explorador de ficheiros da interface, «Guardar na NAS», uma pasta fixada com sub-pastas por abrir), `Engine::populate` lista a NAS, cria os placeholders que faltam e marca a pasta como aberta, e daí em diante o `poll_dir` mantém-na a par da NAS como as outras. O `fetch_placeholders` só entrega ao Windows os nomes que ainda não existem no disco.

Para ver o estado de um item, adotar uma pasta, marcar um ficheiro como enviado, hidratar e fixar usam-se handles normais (`read_handle`, `data_handle`, `write_handle`), que funcionam com o item aberto noutros programas. Só a troca do conteúdo de um ficheiro que mudou na NAS abre com oplock e em exclusivo (`CfOpenFileWithOplock`): largar dados exige o ficheiro só para o motor, e com ele aberto noutro programa fica para a volta seguinte. Um ficheiro só conta como sincronizado se o Windows o disser *e* tiver o tamanho da versão guardada no placeholder: há escritas (por handles que só acrescentam) que o Windows não assinala.

## Serviço e interface

### `logging.rs`

Escreve no stderr no formato do env_logger e guarda as últimas 1000 linhas de nível info ou superior para `/api/log`.

### `ui.rs`

hyper 1 (HTTP/1.1) em `127.0.0.1:ui_port`. A página é servida sem autenticação; tudo em `/api/*` exige o header `x-nasyncloud-token`.

| Pedido | O que faz |
|---|---|
| `GET /api/ping` | Saúde (usado pelo `nasyncloud ui` para encontrar uma instância a correr) |
| `GET /api/state` | Versão, plataforma, definições (sem password), fase, erro, `MountInfo`, caminhos |
| `GET /api/log?after=N` | Linhas novas do registo |
| `POST /api/test` | `{url, username, password?, accept_invalid_certs}` → `{ok, items, error, cert_problem}` |
| `POST /api/config` | `{config, password?, store_in_file}` → grava e (re)monta; `{ok:false, keyring_error}` se o cofre falhar |
| `POST /api/mount`, `/api/unmount` | Arrancar/parar (assíncrono; acompanhar por `/api/state`) |
| `GET /api/browse?path=` | `Listing { path, this, entries[] }` com estado por entrada |
| `POST /api/pin` | `{path, on}` |
| `POST /api/free` | `{path}` → `{freed}` |
| `POST /api/refresh` | `{path}` |
| `GET /api/conflicts` | `{conflicts: [{id, path, copy, at, mine_size, mine_mtime, nas}]}` |
| `POST /api/conflicts` | `{id, keep: "mine" \| "nas" \| "both"}` → `{outcome: "done" \| "changed" \| "sending" \| "nas_gone" \| "gone"}` |
| `POST /api/open` | Abre o caminho local com a aplicação por omissão |
| `POST /api/quit` | Termina o processo |

Proteções:

- O token tem 256 bits e está em `ui.json`, com permissões 0600, ao lado das definições; mantém-se entre arranques.
- O `Host` tem de ser `127.0.0.1`, `localhost` ou `[::1]` com a porta certa; caso contrário, 421.
- A página tem CSP sem origens externas e `frame-ancestors 'none'`.
- O corpo dos pedidos está limitado a 64 KiB.
- Caminhos com `.`/`..` são recusados.

### `ui/index.html`

HTML, CSS e JS sem build nem dependências. Rotas por hash: `#/`, `#/ficheiros/<caminho>`, `#/atividade`, `#/definicoes`. Na primeira abertura, o token chega no fragmento (`#t=…`), passa para o `localStorage` e é tirado do URL. Sondagens: estado a cada 2 s, registo a cada 2 s, listagem a cada 5 s na secção Ficheiros, e mais rápido durante alguns segundos depois de uma ação.

## Testes

- `cargo test`: testes unitários e de integração (`tests/webdav.rs`, `tests/vfs.rs`) contra um servidor WebDAV em memória (`tests/common/mod.rs`).
- `tests/e2e.sh` (Linux e macOS): mount a sério (FUSE ou NFS) contra o rclone ou o `examples/fake_nas.rs`, mais a API da interface.
- `tests/e2e_windows.rs` (Windows): o equivalente para o motor do Windows, como teste de integração do `cargo test`. Corre sozinho no runner `windows-latest` do GitHub Actions: em cada push para `main`, em cada pull request e à mão (`.github/workflows/ci.yml`), e antes de cada Release (`release.yml`). À mão, num Windows: `cargo build --example fake_nas && cargo test --test e2e_windows -- --nocapture`.

O e2e do Windows arranca o `fake_nas` sobre uma pasta e o `nasyncloud serve` com uma configuração de teste (password por `NASYNCLOUD_PASSWORD`), tudo em `target/tmp/e2e-windows`, e verifica de três lados: o sistema de ficheiros (com a própria Cloud Files API a dizer se um item é um placeholder sincronizado), a API local (token do `ui.json`) e a pasta e o registo de pedidos do `fake_nas`. Como o `tests/e2e.sh`, corre todas as verificações e falha no fim se alguma falhou.

Primeira parte, uma NAS normal (portas 5025 e 18750):

- a interface lista o cimo e uma sub-pasta antes de qualquer outro programa abrir a pasta;
- o que está na NAS aparece como placeholder (só online), sem descarregar nada, e o conteúdo chega ao ler;
- ficheiros e pastas criados cá são enviados, criados na NAS e adotados (placeholder sincronizado, `cached` em `/api/browse`), sem ficarem em `info.errors`;
- alterar ficheiros da NAS: acrescentar (com um handle que só acrescenta, num ficheiro só online e num já descarregado), escrever no início, substituir o conteúdo;
- o mesmo que enviar e adotar com o item aberto por outro processo (o próprio teste): um ficheiro aberto com partilha total, aberto sem deixar escrever, com um bloqueio de bytes e sob um oplock exclusivo que o outro programa larga quando lho pedem; uma pasta vigiada como o Explorador faz (`ReadDirectoryChangesW`) e uma aberta sem deixar escrever;
- alterações feitas na NAS por outros (ficheiro ou pasta novos, ficheiro alterado, ficheiro ou pasta apagados), «Libertar espaço» e «Manter sempre neste dispositivo» (também numa pasta que ninguém abriu, com sub-pastas), renomear, mover e apagar;
- parar e voltar a arrancar: nada é enviado outra vez, não há cópias de conflito; `nasyncloud unregister`.

Segunda parte, uma NAS com o cimo fechado como o da UGOS (`fake_nas --closed-top`, portas 5026 e 18751; só com `NASYNCLOUD_E2E_KNOWN_FOLDERS=1`, porque move o Ambiente de trabalho de quem corre o teste):

- `GET /api/folders` oferece o cimo e as pastas da NAS; `POST /api/folders` com `place` vazio devolve `refused: true` sem mover nada nem criar nada, e o cimo deixa de ser oferecido;
- com `place` igual a uma pasta da NAS, o Ambiente de trabalho passa para lá (`SHSetKnownFolderPath`, com o Explorador do runner a correr), o que lá estava é enviado e adotado, e a pasta da NAS continua a mostrar o que já tinha;
- `on: false` devolve o Ambiente de trabalho ao sítio habitual, e os ficheiros ficam na NAS.

O que não cobre: um Windows de cliente (o runner é um Windows Server 2025), a UGOS a sério, a app da bandeja e o instalador.
