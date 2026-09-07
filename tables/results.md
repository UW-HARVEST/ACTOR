# Translation Results

Auto-generated from validated `result.json` and `summary.json` files.

## B01_organic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 37/38 | 772/772 | 2455 | 3771 | 1699 | 45.1% |
| codex | 38/38 | 775/775 | 2455 | 2372 | 1203 | 50.7% |
| kiro | 38/38 | 775/775 | 2455 | 3635 | 1669 | 45.9% |
| kiro-translate | 38/38 | 775/775 | 2455 | 3474 | 1516 | 43.6% |

## B01_synthetic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 84/85 | 392/392 | 2597 | 8749 | 1228 | 14.0% |
| codex | 85/85 | 393/393 | 2597 | 3680 | 969 | 26.3% |
| kiro | 84/85 | 388/388 | 2597 | 9435 | 1292 | 13.7% |
| kiro-translate | 85/85 | 393/393 | 2597 | 8376 | 1052 | 12.6% |

## B02_organic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 43/44 | 261/261 | 23072 | 30707 | 21797 | 71.0% |
| codex | 42/44 | 257/261 | 23072 | 23400 | 17330 | 74.1% |
| kiro | 42/44 | 251/251 | 23072 | 28832 | 21197 | 73.5% |
| kiro-translate | 43/44 | 261/261 | 23072 | 27978 | 20727 | 74.1% |

## B02_synthetic

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 39/42 | 1001/1025 | 9657 | 14702 | 4188 | 28.5% |
| codex | 38/42 | 993/1017 | 9657 | 10697 | 3276 | 30.6% |
| kiro | 39/42 | 998/1022 | 9657 | 14587 | 3650 | 25.0% |
| kiro-translate | 40/42 | 1001/1001 | 9657 | 13875 | 3489 | 25.1% |

## P00_perlin_noise

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 1/1 | 30/30 | 380 | 847 | 4 | 0.5% |
| codex | 1/1 | 30/30 | 380 | 328 | 20 | 6.1% |
| kiro | 1/1 | 30/30 | 380 | 2141 | 14 | 0.7% |
| kiro-translate | 1/1 | 30/30 | 380 | 894 | 0 | 0.0% |

## P01_sphincs_plus

| Agent | Cases Passed | Vectors Passed | C LOC | Rust LOC | Unsafe Lines | Unsafe % |
|-------|-------------|----------------|-------|----------|-------------|----------|
| claude | 128/128 | 128/128 | 6611 | 6720 | 651 | 9.7% |
| codex | 128/128 | 128/128 | 6611 | 4496 | 825 | 18.3% |
| kiro | 128/128 | 128/128 | 6611 | 6951 | 5728 | 82.4% |
| kiro-translate | 1/128 | 1/1 | 6611 | 6950 | 5728 | 82.4% |

## Summary: Cases Passed

| Battery | claude | codex | kiro | kiro-translate |
|---------|------|------|------|------|
| B01_organic | 37/38 | 38/38 | 38/38 | 38/38 |
| B01_synthetic | 84/85 | 85/85 | 84/85 | 85/85 |
| B02_organic | 43/44 | 42/44 | 42/44 | 43/44 |
| B02_synthetic | 39/42 | 38/42 | 39/42 | 40/42 |
| P00_perlin_noise | 1/1 | 1/1 | 1/1 | 1/1 |
| P01_sphincs_plus | 128/128 | 128/128 | 128/128 | 1/128 |

