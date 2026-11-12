# Arquitetura

## Visão geral

```
            browser ──HTTP 127.0.0.1──▶ ui.rs ──┐
                                                 ▼
  nasdrive mount / ui ──▶ service.rs ──▶ Mounted (unix.rs | win.rs)
                                                 │
                   Linux/macOS: fuse.rs ◀── vfs/ (árvore, cache, índice redb)
                   Windows:     Cloud Files API ◀── win.rs (placeholders no NTFS)
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

Antes de cada upload é feito um `stat` remoto. Se a versão na NAS não for a versão-base local, cria-se uma cópia de conflito: o ficheiro local muda de nome e a versão da NAS reaparece. Caso contrário, o upload vai para `.<pid>-<ns>-<seq>.nasdrive-part` e depois MOVE com overwrite. Se o servidor recusar o MOVE, passa a upload direto (e marca `overwrite_ok` quando um upload direto interrompido pode ter deixado meia cópia nossa). No fim, verifica o tamanho e adota a versão nova.

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

### FUSE (`fuse.rs`)

Cada pedido do kernel é passado ao runtime Tokio. Os atributos são guardados em cache 1 s. `blocks` reflete os bytes realmente locais (`du`). Os comandos da CLI (`pin`, `free`, `status`...) falam com o mount por atributos estendidos `user.nasdrive.*`, que não aparecem em `listxattr` para o `cp -a` não os copiar. No macOS: `noappledouble`, `noapplexattr`.

## Motor Windows (`win.rs`)

O próprio NTFS guarda a árvore, em placeholders. A identidade de cada placeholder é a `Version` em JSON. O registo da sync root é feito por (endereço, utilizador), com hidratação `Full`, população `Full`, desidratação automática permitida e fixar permitido.

Callbacks:

- `fetch_placeholders`: lista a pasta.
- `fetch_data`: GET com Range, em blocos de 4 KiB alinhados (≥256 KiB por escrita) e com progresso.
- `delete` / `rename`: são feitos primeiro na NAS e só depois confirmados ao Windows.
- `dehydrate`: aceite sempre.
- `state_changed`: o vigia da pasta põe caminhos na fila.

A thread `nasdrive-sync` tem a ligação e o ciclo de trabalho. Processa a fila depois de 2 s de acalmia: envia ficheiros novos ou modificados e converte-os em placeholders, e trata de fixar/desafixar. Sonda as pastas já abertas a cada `poll_interval`, ou a cada 15 s quando está offline. As operações feitas pelo próprio motor ficam marcadas em `ours`, para não voltarem como eco.

## Serviço e interface

### `logging.rs`

Escreve no stderr no formato do env_logger e guarda as últimas 1000 linhas de nível info ou superior para `/api/log`.

### `ui.rs`

hyper 1 (HTTP/1.1) em `127.0.0.1:ui_port`. A página é servida sem autenticação; tudo em `/api/*` exige o header `x-nasdrive-token`.

| Pedido | O que faz |
|---|---|
| `GET /api/ping` | Saúde (usado pelo `nasdrive ui` para encontrar uma instância a correr) |
| `GET /api/state` | Versão, plataforma, definições (sem password), fase, erro, `MountInfo`, caminhos |
| `GET /api/log?after=N` | Linhas novas do registo |
| `POST /api/test` | `{url, username, password?, accept_invalid_certs}` → `{ok, items, error, cert_problem}` |
| `POST /api/config` | `{config, password?, store_in_file}` → grava e (re)monta; `{ok:false, keyring_error}` se o cofre falhar |
| `POST /api/mount`, `/api/unmount` | Arrancar/parar (assíncrono; acompanhar por `/api/state`) |
| `GET /api/browse?path=` | `Listing { path, this, entries[] }` com estado por entrada |
| `POST /api/pin` | `{path, on}` |
| `POST /api/free` | `{path}` → `{freed}` |
| `POST /api/refresh` | `{path}` |
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
