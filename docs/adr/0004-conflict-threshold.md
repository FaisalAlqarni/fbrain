# 0004: Conflict threshold

Status: accepted (task 06).

## Decision

`conflict_threshold` defaults to 0.50: the cosine similarity between a proposal body and a note's best
chunk above which the note is a conflict candidate. Both vectors come from the default model at 256 dimensions;
the proposal is embedded as a document.

## Measurement

Ten hand-made proposal texts against the 55-note ext4 bench copy: five that restate or contradict a decision
already in the notes, and five on unrelated topics (cooking, sport, travel, plants, cycling). The texts stay in
the private bench folder. Best-chunk cosine of the top note:

| Group | Values |
|---|---|
| Related | 0.646, 0.614, 0.647, 0.554, 0.569 |
| Unrelated | 0.379, 0.407, 0.435, 0.448, 0.413 |

Any threshold in (0.448, 0.554] flags all five related texts and none of the unrelated ones. 0.50 sits in the
middle of that gap.

## Limits

Ten texts from one corpus. The gap is 0.10 wide, so a brain with different notes may need `conflict_threshold`
set in `.fbrain.toml`. Unrelated texts here are far from the notes; near-miss topics were not tested.
