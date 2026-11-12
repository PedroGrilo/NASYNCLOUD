# Decisões

Registo do porquê das escolhas principais (2026-11).

1. **WebDAV como protocolo.** O DH2300 não tem Docker oficial, por isso nada pode correr na NAS (nem Nextcloud, nem Resilio). A UGOS já traz WebDAV (5005/5006), que dá ETags, datas e leituras por intervalos, o necessário para ficheiros a pedido. SMB fica para depois, atrás do mesmo trait `Remote`.
2. **Linux/macOS com FUSE e cache própria**, em vez de montar e confiar no cache do sistema. Só assim há "manter offline", "libertar espaço", trabalho offline com fila de envio e estado por ficheiro.
3. **Windows com a Cloud Files API** (a do OneDrive), em vez de WinFsp. Não precisa de instalar drivers e dá de graça os ícones e os menus do Explorador.
4. **macOS com um servidor NFS local** (desde a 1.1.0, 2026-11). Até à 1.0.0 usava-se o macFUSE (motor kext), que obrigava a instalar uma extensão de sistema, autorizá-la e reiniciar. Alternativas vistas: File Provider (o que o OneDrive usa: dá ícones no Finder, mas exige extensão em Swift e, na prática, a conta Apple Developer), FSKit (macOS 26+, a extensão tem de ser ativada nas Definições, sem xattrs nem bloqueios, ainda imaturo) e FUSE-T (continua a ser algo para instalar). O NFS local reaproveita o `Vfs` todo, não precisa de nada instalado e o `mount_nfs` funciona sem administrador. Custos: os xattrs passam a ficheiros `._*` (ficam em memória), os comandos da CLI vão pela API local e um serviço morto deixa o mount pendurado até ao próximo arranque. O File Provider continua a ser o passo seguinte se houver conta Apple Developer.
5. **Uploads para nome temporário + MOVE.** Uma falha a meio nunca deixa um ficheiro cortado. Se o servidor não suportar MOVE por cima, passa sozinho a upload direto.
6. **Conflitos: ficam os dois.** O ficheiro local passa a `nome (conflict DATA).ext` e o da NAS mantém o nome. Nunca se perde trabalho e nunca se escolhe por heurística.
7. **Uma edição ganha a uma remoção remota.** Se um ficheiro foi apagado na NAS mas alterado aqui, volta a subir.
8. **Índice em redb com registos JSON.** Transações atómicas, um só ficheiro, sem dependências nativas. O JSON com defaults deixa acrescentar campos sem invalidar caches existentes.
9. **Interface web local servida pelo próprio binário** (como o Syncthing), em vez de GUI nativa ou Tauri. Funciona igual nos três sistemas, sem dependências gráficas. Está sempre disponível quando o NASYNCLOUD corre como serviço, é testável por HTTP e o dono, programador frontend, consegue mexer-lhe sem passo de build. Um invólucro Tauri ou ícone de bandeja pode vir por cima, sem mudar a API.
10. **Segurança da interface com token + verificação do Host** em vez de login. É uma ferramenta pessoal em localhost; o token impede outras páginas e outros utilizadores de a usar, e a verificação do Host impede DNS rebinding.
11. **Password no cofre do sistema por omissão.** Ficheiro de definições só com consentimento explícito; `NASYNCLOUD_PASSWORD` para serviços.
12. **Idiomas.** Interface e documentação em pt-PT (para o dono); código, logs e CLI em inglês.
13. **App de bandeja com Tauri, à volta do serviço** (2026-11). Fica como processo separado (`nasyncloud serve`) e as páginas são as mesmas que o serviço serve. Assim a app continua viva quando o serviço falha (para o dizer), não duplica a interface, e a mesma casca serve o Windows e o macOS. A app e as páginas continuam iguais.
