---
title: front matter is hidden
---

# mdview demo

A **fast**, _lightweight_ Markdown viewer with ~~heavy~~ GitHub-style rendering. Inline `code`, <kbd>Ctrl</kbd>+<kbd>F</kbd>, autolinks like https://github.com and emoji shortcodes :rocket: :sparkles:.

> [!NOTE]
> Useful information that users should know, even when skimming content.

> [!TIP]
> Helpful advice for doing things better or more easily.

> [!IMPORTANT]
> Key information users need to know to achieve their goal.

> [!WARNING]
> Urgent info that needs immediate user attention to avoid problems.

> [!CAUTION]
> Advises about risks or negative outcomes of certain actions.

## Code

```rust
use std::collections::HashMap;

/// Counts words in a string.
fn word_count(text: &str) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}
```

```python
import json

def load(path: str) -> dict:
    """Load a JSON config file."""
    with open(path) as f:
        return json.load(f)  # TODO: validate
```

```bash
# Build on RHEL 8 / ppc64le
sudo dnf install -y webkit2gtk3-devel gtk3-devel
cargo build --release && ./target/release/mdview README.md
```

```diff
- old line
+ new line
```

## Tables

| Feature | Status | Notes |
|:--------|:------:|------:|
| GFM tables | ✅ | aligned |
| Task lists | ✅ | below |
| Footnotes | ✅ | [^1] |
| Alerts | ✅ | GitHub style |

## Lists

- [x] Render Markdown
- [x] Syntax highlighting
- [ ] Take over the world
  1. Nested ordered
  2. Items
     - and deeper

> A blockquote with **emphasis** and a [link](https://example.com).
>
> — Someone

<details>
<summary>Click to expand</summary>

Hidden content with `code`.

</details>

---

### Images

![Logo](logo.svg)

[^1]: Footnotes work too.
