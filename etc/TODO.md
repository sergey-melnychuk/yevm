web:

- when clicked on address/hash/value, all the same occurrences are highlighted in yellow, including the one that was clicked at; when new item is clicked, prefious highlighting is discarded
- add custom call form: fields for from, to, gas (limit + gas market?), eth, calldata (all hex strings) - make a modal window for the form (abi fetching?)
- add query param 'call' for base64-encoded payload for custom call (link is copyable from the custom call form modal window)
- both 'call' and 'tx' query params are provided, fill both forms but don't execute anything - user will click either 'Simulate' for tx hash/block:index or 'Custom' for executing base64-decoded call json
- consider wrapping value part of Get/Put 'address[slot] = {old -> new | val}' to a new line when value part is too long, rn horizontal overflow is hidden, and I don't want horizontal scrolling

- allow pre-execution prior transactions in the block for the selected transaction (e.g. simulating tx 42 will execute txs 0-41, and then execute tx 42 on aggregated state); this can take a long time for many prior txs, so needs to be used wisely - but in the end of the day the user decides; the progress bar of prior txs executed is necesary to show that page is making progress and is not hanging

---

0x43fceff6481ca75518e45a32d642f63644707d6502fd6ff8b266a5d9ba076f17 [type:3]: FAIL=24938235:217 [1/1, 3101ms/594ms, fetches:46/2507ms]
gas: have 389170 want 414114 [-24944]
AND
./target/release/replay 24938235:217
Begin: 24938235 / 0xbfe807dada4e6cb499497236460e6e6211b1c3742c4a5bf73ee06cbbba128ef1
0x43fceff6481ca75518e45a32d642f63644707d6502fd6ff8b266a5d9ba076f17 [type:3]: FAIL=24938235:217 [1/1, 3043ms/790ms, fetches:46/2253ms]
gas: have 389170 want 414114 [-24944]

---

0x43ffef9f80b0f1b1c65ad2a59bb497f29ca607ba99aab709c3b2fa91faeba9b3 [type:2]: FAIL=24935693:31 [1/1, 7815ms/569ms, fetches:123/7246ms]
gas: have 1901504 want 1907544 [-6040]
BUT
./target/release/replay 24935693:31
Begin: 24935693 / 0x0fc315b2a8416bc4145e2b95c71ef5c4628e1b8bb14513100afa698343bbdc52
0x43ffef9f80b0f1b1c65ad2a59bb497f29ca607ba99aab709c3b2fa91faeba9b3 [type:2]: OK [1/1, 1907544 gas, 8904ms/479ms, fetches:123/8425ms]

0x4cd30dc1e591573fd8c029a9bb75433c116da7bbb801c9aa3e3c308bb83a3e06 [type:2]: FAIL=24935758:95 [1/1, 2310ms/368ms, fetches:36/1942ms]
gas: have 314259 want 312102 [+2157]
BUT
./target/release/replay 24935758:95 
Begin: 24935758 / 0xcbf879e7b91b0f36207e2d15c2c276e13f9e11b6b97d22022a650bb4e172aa92
0x4cd30dc1e591573fd8c029a9bb75433c116da7bbb801c9aa3e3c308bb83a3e06 [type:2]: OK [1/1, 312102 gas, 2113ms/385ms, fetches:36/1728ms]

0x196135cd96d5425a709dab4dabe7c577610753c3827a25e27d5186dc21d2b72c [type:2]: FAIL=24938071:119 [1/1, 2833ms/445ms, fetches:53/2388ms]
gas: have 423169 want 419409 [+3760]
BUT
./target/release/replay 24938071:119
Begin: 24938071 / 0xbf7d40424e0e3a28f73291b7494ad4636dee8a7bbc1c25135c436d00c1adce0f
0x196135cd96d5425a709dab4dabe7c577610753c3827a25e27d5186dc21d2b72c [type:2]: OK [1/1, 419409 gas, 2562ms/360ms, fetches:53/2202ms]

0xeafcfc6b4019ae0209b0abc7c1e28e7642c565344c9c94e2f29521ec0137b77a [type:2]: FAIL=24938116:64 [1/1, 2676ms/421ms, fetches:53/2255ms]
gas: have 404278 want 388699 [+15579]
BUT
./target/release/replay 24938116:64 
Begin: 24938116 / 0x3e53f0157a1751ec05fb1b3a5baaa96508b9fe234b58ae30cbc9a3e0e7c98ed7
0xeafcfc6b4019ae0209b0abc7c1e28e7642c565344c9c94e2f29521ec0137b77a [type:2]: OK [1/1, 388699 gas, 2910ms/488ms, fetches:53/2422ms]

0xe9403969ce3304457a8a3602a5fe8035fd6611f632240fc0477fed761bf51324 [type:2]: FAIL=24938235:144 [1/1, 1586ms/358ms, fetches:23/1228ms]
gas: have 228325 want 228315 [+10]
BUT
./target/release/replay 24938235:144
Begin: 24938235 / 0xbfe807dada4e6cb499497236460e6e6211b1c3742c4a5bf73ee06cbbba128ef1
0xe9403969ce3304457a8a3602a5fe8035fd6611f632240fc0477fed761bf51324 [type:2]: OK [1/1, 228315 gas, 1528ms/484ms, fetches:23/1044ms]

0x15809bea3f5664afe05c0d1676fbb190edbccb6043ad1d87d992a1b7caad6ac2 [type:2]: FAIL=24938261:35 [1/1, 3952ms/502ms, fetches:81/3450ms]
gas: have 1635078 want 1633920 [+1158]
BUT
./target/release/replay 24938261:35
Begin: 24938261 / 0x2159e1496e5d53998cf20577c9c028058bbbe53529eeacc4530a4113ce4d63d0
0x15809bea3f5664afe05c0d1676fbb190edbccb6043ad1d87d992a1b7caad6ac2 [type:2]: OK [1/1, 1633920 gas, 4006ms/473ms, fetches:81/3533ms]

0x1a3d730f5d0d7476de7467043da4c7d12d129f36edc46f0a906b21dd50ea134c [type:2]: FAIL=24938069:1 [2/374, 3132ms/488ms, fetches:53/2644ms]
gas: have 396901 want 333499 [+63402]
BUT
./target/release/replay 24938069:1
Begin: 24938069 / 0x4a2469cf37fe68a9d3687e2f796f301e3b9e1c7be30b0360f5af69110f7218bb
0x1a3d730f5d0d7476de7467043da4c7d12d129f36edc46f0a906b21dd50ea134c [type:2]: OK [1/1, 333499 gas, 2777ms/481ms, fetches:53/2296ms]

0x78293b51147dfc27e2c96b9303676f1a584192ce20ffca13e08a0addfe664eb0 [type:2]: FAIL=24938068:1 [1/1, 3259ms/455ms, fetches:51/2804ms]
 ok: have 0 want 1
gas: have 299913 want 294274 [+5639]
BUT
./target/release/replay 24938068:1            
Begin: 24938068 / 0x71ab8b8ebe8d940a7b951e45875e049857146becb1411288d88d2f04b77fab2c
0x78293b51147dfc27e2c96b9303676f1a584192ce20ffca13e08a0addfe664eb0 [type:2]: OK [1/1, 294274 gas, 2951ms/465ms, fetches:51/2486ms]

===

The 9 failures fall into 2 categories:

A — 1 tx, type:3, gas mismatch
0x43fceff6... off by -24944. Probably the blob gas charging issue. Reproduced by single-tx call.

B — 8 txs, type:2, gas mismatch
Varying offsets: -6040, +2157, +3760, +15579, +10, +1158, +63402, +5639.
One also has ok: have 0 want 1 (wrong revert outcome).
All 8 transactions succeed when run in standalone mode (single-tx).
