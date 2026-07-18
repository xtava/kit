---
title: Kit Markdown Showcase
purpose: Exercise the elements supported by kit render
---

# Kit Markdown Showcase

Use this document to inspect Markdown layout, styling, wrapping, and syntax highlighting in
`kit render`.

## Inline text

Plain text can contain **strong emphasis**, *emphasis*, ~~strikethrough~~, `inline code`,
superscript such as x^2^, and subscript such as H~2~O.

This deliberately long paragraph exercises width-aware wrapping while preserving **styles**,
[links](https://example.com), and `inline code` as the terminal becomes narrower or wider.

This line ends with a hard break.  
This text should begin on the next rendered line.

## Headings

### Third-level heading

#### Fourth-level heading

##### Fifth-level heading

###### Sixth-level heading

## Lists and tasks

- Unordered item
- Item with nested content
  1. First nested ordered item
  2. Second nested ordered item
     - Deeply nested item
- Final unordered item

1. First ordered item
2. Second ordered item
3. Third ordered item

- [x] Completed task
- [ ] Pending task

## Blockquotes and alerts

> A normal blockquote can wrap across multiple terminal lines while retaining its marker and
> indentation.

> [!NOTE]
> Notes provide useful supporting information.

> [!TIP]
> Tips highlight a helpful shortcut.

> [!IMPORTANT]
> Important information deserves attention.

> [!WARNING]
> Warnings describe a meaningful risk.

> [!CAUTION]
> Cautions describe a potentially harmful outcome.

## TypeScript

```typescript
interface User {
  id: string;
  displayName: string;
  roles: readonly string[];
}

const formatUser = (user: User): string => {
  const roleCount = user.roles.length;
  return `${user.displayName} (${roleCount} roles)`;
};

// Keywords, types, strings, template expressions, and comments should use distinct colors.
const ada: User = {
  id: "user-1",
  displayName: "Ada",
  roles: ["admin", "author"],
};

console.log(formatUser(ada));
```

## TSX

```tsx
type GreetingProps = {
  name: string;
  excited?: boolean;
};

export function Greeting({ name, excited = false }: GreetingProps) {
  return (
    <section className="greeting" data-excited={excited}>
      <h2>Hello, {name}{excited ? "!" : "."}</h2>
    </section>
  );
}
```

## Other code fences

```rust
fn main() {
    let message = "Rust highlighting still works";
    println!("{message}");
}
```

```json
{
  "name": "kit",
  "features": ["markdown", "syntax-highlighting"],
  "enabled": true
}
```

```toml
[render]
theme = "nord"
show_ignored = true
```

```unknown-language
Unknown languages remain readable using the fallback code style.
```

```
An untagged code fence also uses the fallback code style.
```

## Table

| Element | Alignment | Expected rendering |
| :--- | :---: | ---: |
| Heading | Left | Styled |
| Code | Center | Highlighted |
| Table | Right | Bordered |

## Links and images

This is an [external link](https://example.com) and this is a
[repository-relative link](../docs/render.md).

![Remote image description](https://example.com/markdown-showcase.png "Images stay inert")

## Definition list

Markdown renderer
: Converts parsed Markdown events into width-aware Ratatui text.

Syntax highlighter
: Converts recognized source-language tokens into themed terminal spans.

## Footnotes

Footnotes keep supporting information out of the main sentence.[^render]

Named references are supported too.[^named]

[^render]: `kit render` displays this definition with its reference marker.
[^named]: This is a named footnote definition.

## Math

Inline math: $E = mc^2$.

Display math:

$$
\sum_{i=1}^{n} i = \frac{n(n+1)}{2}
$$

## Wiki link

The parser recognizes a wiki-style link such as [[docs/render.md]].

## Raw HTML

<aside data-kind="example">Raw HTML is displayed as inert text and is never executed.</aside>

---

End of showcase.
