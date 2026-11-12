# Changelog

## 0.1.0 — 2026-11-12

- Cliente WebDAV (PROPFIND, GET por intervalos, PUT, MKCOL, MOVE, DELETE, quota), com contornos para servidores que ignoram `Range` ou respondem 405 a MKCOL.
- Motor Linux/macOS (FUSE): ficheiros a pedido com cache por intervalos, manter offline, libertar espaço, limite de cache com LRU, uploads em segundo plano, trabalho offline com fila persistente, conflitos com cópia, índice com recuperação após crash.
- Motor Windows (Cloud Files API): placeholders, hidratação a pedido, envio de alterações locais, espelho de apagar/renomear, sondagem da NAS. Compila; ainda não testado.
- Linha de comandos: `ui`, `setup`, `mount`, `pin`, `unpin`, `free`, `status`, `ls`, `refresh`, `unmount`, `unregister`.
- Interface web local (`nasdrive ui`): estado, explorador com estado por ficheiro e ações, atividade, definições com teste de ligação. Protegida por token e verificação do Host.
- Testes: 28 automáticos (unitários e de integração contra um servidor WebDAV em memória) e 64 verificações de ponta a ponta num mount FUSE real.
