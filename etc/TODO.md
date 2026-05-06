TODO:

- add custom state overrides for tx simulation: address[slot] = value; make base64-encoded query param 'overrides' for it; consider allowing override nonce, value and code

DONE:

- when clicked on address/hash/value, all the same occurrences are highlighted in yellow, including the one that was clicked at; when new item is clicked, prefious highlighting is discarded
- add custom call form: fields for from, to, gas (limit + gas market?), eth, calldata (all hex strings) - make a modal window for the form (abi fetching?)
- add query param 'call' for base64-encoded payload for custom call (link is copyable from the custom call form modal window)
- both 'call' and 'tx' query params are provided, fill both forms but don't execute anything - user will click either 'Simulate' for tx hash/block:index or 'Custom' for executing base64-decoded call json
- allow pre-execution prior transactions in the block for the selected transaction (e.g. simulating tx 42 will execute txs 0-41, and then execute tx 42 on aggregated state); this can take a long time for many prior txs, so needs to be used wisely - but in the end of the day the user decides; the progress bar of prior txs executed is necesary to show that page is making progress and is not hanging
- consider wrapping value part of Get/Put 'address[slot] = {old -> new | val}' to a new line when value part is too long, rn horizontal overflow is hidden, and I don't want horizontal scrolling
