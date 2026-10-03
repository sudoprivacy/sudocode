Inline tokens: `source ~/.zshrc` and `cc-sudo`.

```rust
// Rust colors
fn greet(name: &str) -> String {
    format!("Hello {name}: {}", 42)
}
```

```typescript
// TypeScript colors
const enabled: boolean = true;
const message = `Hello ${enabled}`;
```

```python3
# Python colors
def greet(name: str):
    return f"Hello {name}", 42
```

```shell
# Shell colors
export GREETING="hello"
printf '%s\n' "$GREETING"
```

```rust
/* multiline color
 comment continuation */
let answer = 42;
```

```unknown-language
Unhighlighted fallback stays plain.
```

Color reference done.
