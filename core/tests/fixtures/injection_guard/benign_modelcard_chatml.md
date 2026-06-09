# Qwen2.5-0.5B-Instruct

This model uses the ChatML prompt format. Each conversation turn is wrapped
in special control tokens. A system turn looks like:

```
<|im_start|>system
You are a helpful assistant.<|im_end|>
<|im_start|>user
Hello!<|im_end|>
<|im_start|>assistant
```

When you call `tokenizer.apply_chat_template(...)`, these tokens are added
automatically. Do not type them by hand in normal use.
