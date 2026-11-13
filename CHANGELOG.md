# Changelog

## 1.3.1 — 2026-11-13

- **macOS: copiar pastas grandes já não falha com «erro -36».** O serviço arrancava com o limite de 256 ficheiros abertos que o macOS dá às apps, e numa cópia grande (um `node_modules`, um `venv`) esgotava-o: cada ficheiro tocado ficava aberto mais 1 a 5 s, e cada envio usava ainda um socket. O Finder mostrava «não foi possível ler ou escrever alguns dados» e a Atividade mostrava «the NAS is unreachable … Too many open files». Agora o serviço sobe o limite ao arrancar (até 65 536, conforme o sistema deixar) e nunca guarda mais de 128 ficheiros abertos por pedido do NFS: os parados fecham-se logo, a começar pelos mais antigos. Também se aplica ao Linux (o limite).

## 1.3.0 — 2026-11-13

- **macOS: copiar muitos ficheiros ficou 4 a 5 vezes mais rápido.** O servidor NFS (`nfsserve`) percorria, em cada pedido, a lista de todos os pedidos do último minuto. Quanto mais depressa se copiava, mais lento ficava cada pedido: o `react-app` com `node_modules` (6 684 entradas) demorava 132 s e agora demora ~30 s. Também desapareceram os erros ocasionais «could not copy extended attributes» que isto provocava. O `nfsserve` passa a vir no repositório (`vendor/nfsserve`) com essa correção.
- **Linux e macOS: ligações simbólicas (symlinks).** Copiar uma pasta com symlinks (um `node_modules`, por exemplo, com `.bin/jsesc -> ../jsesc/bin/jsesc`) falhava no Finder com «não foi possível ler ou escrever alguns dados». O WebDAV não tem symlinks, por isso ficam só neste computador, como os itens excluídos: aparecem e funcionam aqui, sobrevivem a reinícios, mas não vão para a NAS.
- **Envios em simultâneo: automático ou manual.** No modo automático (o novo valor por omissão) o número é escolhido pelo tamanho dos ficheiros: até 64 pequenos ao mesmo tempo (onde o tempo vai nas idas e voltas à NAS), menos à medida que crescem, e 2 de cada vez acima de 32 MB. No manual, o limite subiu de 16 para 64. Linux e macOS (o Windows envia um de cada vez).

## 1.2.1 — 2026-11-12

- **Windows: acabou a cascata de cópias de conflito.** Depois de enviar um ficheiro, marcá-lo como sincronizado podia falhar com «O identificador é inválido» (0x80070006) quando outro programa o abria logo a seguir (um antivírus a analisar um `.zip`, por exemplo). A tentativa seguinte tomava o próprio envio por uma alteração na NAS e criava mais uma cópia, `(conflict …) (conflict …)`, sem fim. Agora a marcação usa um handle normal do ficheiro, e um ficheiro que já está na NAS com o mesmo tamanho e a mesma data conta como nosso.
- Os nomes das cópias de conflito já não acumulam marcadores: fica só um `(conflict …)`.

## 1.2.0 — 2026-11-12

- **macOS sem macFUSE.** O mount passa a ser um servidor NFS local (só em 127.0.0.1, porta aleatória, crate `nfsserve`) montado com o cliente NFS que o macOS já traz (`mount_nfs`, sem administrador). Já não é preciso instalar o macFUSE, autorizar a extensão de sistema nem reiniciar. Linux continua com FUSE.
- macOS: os atributos estendidos funcionam (`ditto`, `cp -p`, etiquetas e quarentena do Finder). Os ficheiros `._*` e `.DS_Store` onde o macOS os guarda ficam em memória e nunca vão para a NAS; perdem-se quando o NASYNCLOUD para.
- macOS: os comandos `pin`, `unpin`, `free`, `status`, `ls` e `refresh` falam com o serviço pela API local (o NFS não deixa passar os xattrs de controlo). Precisam do NASYNCLOUD a correr.
- macOS: um mount deixado por um serviço que morreu é desmontado à força no arranque seguinte, antes de se olhar para a pasta (com o servidor morto, olhar para ela ficava à espera).
- Motor: cada nó tem um contador de alterações (`Attr::change`) que sobe quando uma listagem da NAS o muda. O NFS usa-o nos nanossegundos das datas, para o macOS notar alterações feitas no mesmo segundo ou em pastas cuja data a NAS não muda.
- A app deixou de verificar o macFUSE, e o workflow deixou de o instalar.
- **Excluir da sincronização** (Definições, ou `exclude = [...]` na config): o que tiver esses nomes fica só neste computador. Nunca vai para a NAS e, se a NAS já o tiver, deixa de aparecer aqui (lá fica intacto). Pensado para `.git` e `node_modules`. Aceita nomes (em qualquer sítio, com tudo o que estiver dentro), `*` e `?`, `pasta/` (só pastas) e `/caminho/da/raiz`, sem distinguir maiúsculas. Linux, macOS e Windows. Na lista de ficheiros aparecem como «Só neste computador».
- **Windows: desmontar e sair já não ficam presos em «A parar…».** O trabalho de fundo passava uma volta inteira (enviar tudo o que estava em fila, comparar todas as pastas com a NAS, descarregar pastas fixadas) sem ver o pedido para parar. Agora trabalha em fatias curtas, cancela o envio ou a descarga em curso e nunca espera mais de 20 s; o que ficou por enviar segue no arranque seguinte. O serviço também já não fica preso à saída por uma chamada bloqueada.
- **Windows: ficheiros novos aparecem mais depressa.** Ao comparar as pastas com a NAS, as alterações locais deixam de esperar pelo fim da volta. Uma pasta que se abre no Explorador (ou onde se abre um ficheiro) é relida da NAS ao fim de `dir_ttl_secs`, em vez de esperar pela sondagem dos 5 minutos. Se o vigia de pastas da biblioteca morrer, volta a ligar-se sozinho (antes, as alterações locais só eram vistas na sondagem seguinte). Depois de copiar milhares de ficheiros de uma vez, o varrimento de recurso da biblioteca já não abre todas as pastas da NAS.
- Windows: o que deixou de existir na NAS (ou passou a estar excluído) é retirado daqui sem apagar o que só existe neste computador; antes, uma pasta com algo por enviar ficava inteira, com marcadores de ficheiros que já não existiam.

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
