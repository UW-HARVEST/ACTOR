<!-- markdownlint-disable MD041 -->
Here is a C-to-Rust translation that fails some tests.

Case directory: CASE_DIR_PLACEHOLDER

It contains:
- `translated_rust/c_src/` — original C source (do not modify)
- `translated_rust/src/` — Rust translation (you may modify this to fix it)
- `logs/translation.log` — the LLM conversation that produced this translation
- `test_vectors/` — test inputs and expected outputs

To run the tests:
```
cd TEST_CORPUS_PLACEHOLDER/deployment/scripts/github-actions
PYTHONPATH=. python3 -m runtests.rust \
  --root ROOT_PLACEHOLDER \
  --subset CASE_DIR_PLACEHOLDER \
  --keep-going --verbose
```

Steps:
1. Read the C source and Rust translation
2. Run the tests to see what fails and how
3. Investigate why the translation produces wrong output
4. Attempt a fix in `translated_rust/src/`
5. Run the tests again to verify
6. Explain what was wrong and what you changed
