# nfsserve 0.11.0 (cópia com uma correção)

Cópia da crate [nfsserve](https://github.com/huggingface/nfsserve) 0.11.0, tal como vem do crates.io (licença BSD-3-Clause em `LICENSE`), usada pelo `[patch.crates-io]` do `Cargo.toml` da raiz.

Única alteração: `src/transaction_tracker.rs`. O original percorre, em cada pedido e debaixo de um mutex global, todos os pedidos dos últimos 60 s para apagar os antigos. A copiar muitos ficheiros pequenos (mais de mil pedidos por segundo), cada pedido varria dezenas de milhares de entradas: copiar o `react-app` com `node_modules` (6 684 entradas) demorava 132 s. Agora a limpeza corre no máximo uma vez por segundo.

Ao atualizar o nfsserve: ver se a versão nova já corrige isto; se sim, apagar esta pasta e o `[patch.crates-io]`.
