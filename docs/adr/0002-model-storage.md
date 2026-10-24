# 0002: Model storage form

Status: Superseded by 0003. Measured 2026-10-23: no stored form of the potion model met the 300 MB limit.

## Context

`minishlab/potion-multilingual-128M` has 512 MB of f32 weights (488.6 MiB on disk). The design limit
is 300 MB peak memory per process. fbrain stores the model as `f32`, `f16` or `int8` (`model_dtype`
in `.fbrain.toml`, `FBRAIN_MODEL_DTYPE` for one process). `f16` and `int8` are converted once from the
downloaded f32 file. Int8 uses one scale for the whole matrix, which the pooled-vector normalisation
removes.

## Measurement

One `fbrain search` process on 55 notes, release build, Ryzen 9 9950X3D, 55 GB RAM with about two
thirds in use by other work (load average about 2.2 during the runs). The index was already built,
so the process loads the model, embeds one query and searches. `load_ms` is `Embedder::load` alone.
Peak RSS is the child's `ru_maxrss`.

| Form | Stored | ext4 load | ext4 search | ext4 peak RSS | /mnt/d load | /mnt/d search | /mnt/d peak RSS |
|---|---|---|---|---|---|---|---|
| f32 | 488.6 MiB | 1384 ms | 2014 ms | 1592 MB | 3849 ms | 4674 ms | 1592 MB |
| f16 | 244.3 MiB | 1253 ms | 1927 ms | 1348 MB | 2570 ms | 3413 ms | 1347 MB |
| int8 | 122.2 MiB | 1164 ms | 1745 ms | 1226 MB | 1914 ms | 2651 ms | 1225 MB |

Cold p95 over 50 queries (fresh process per query): int8 1917 ms on ext4 and 2756 ms on /mnt/d;
f32 2768 ms and 4807 ms. Warm in-process p95 is about 1 ms for every form. Recall at 5 is 1.00 for
all three forms on the draft labels; MRR is 0.84 (f32, f16) and 0.82 (int8).

## Decision

No form fits under 300 MB, and none reaches the 300 ms cold target. Int8 has the lowest cold time and
the lowest memory, 1.2 GB, about four times the limit. The stored size shrinks (4x for int8) but memory
does not follow: `model2vec-rs` 0.3.0 decodes the whole matrix to f32 when it loads the model, so
the process holds the f32 matrix plus the file buffer and whatever it copies while building the model.

Nothing in this task changes the default: `model_dtype` stays `f32` until the design is reviewed.
Int8 is the form to carry forward if the model stays, since it is the fastest and smallest here and
its recall matched f32 on this set.

Ways out for the review (none tried here): load the matrix lazily or memory-mapped (needs a change in
or beside `model2vec-rs`), a smaller or PCA-reduced model, or a vocabulary pruned to the tokens the
brain uses.
