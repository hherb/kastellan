# Comparing chat templates

Different model families use different control tokens.

- **ChatML** (Qwen, many OpenAI-compatible models) opens each turn with
  `<|im_start|>` and closes it with `<|im_end|>`.
- **Zephyr-style** templates instead use role tags such as `<|system|>`,
  `<|user|>`, and `<|assistant|>` directly.

Always rely on the model's own `tokenizer_config.json` rather than hard-coding
either convention.
