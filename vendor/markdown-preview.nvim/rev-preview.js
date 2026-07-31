/* rev fork: static Markdown-It renderer + markdown-preview.nvim-style source sync. */
(() => {
  "use strict";

  const current = document.querySelector("#current-markdown");
  const previous = document.querySelector("#previous-markdown");
  const title = document.querySelector("#review-title");
  const status = document.querySelector("#review-status");
  let contentRevision = null;
  let focusKey = null;
  let refreshing = false;
  const previewToken = document.body.dataset.previewToken;
  const MAX_CODE_CHARS = 200000;
  const MAX_MERMAID_CHARS = 100000;

  function sourceLines(md) {
    md.core.ruler.push("rev_source_lines", (state) => {
      for (const token of state.tokens) {
        if (!token.map || token.nesting !== 1 ||
            !["paragraph_open", "heading_open", "list_item_open", "table_open", "tr_open", "blockquote_open"].includes(token.type)) continue;
        token.attrJoin("class", "source-line");
        token.attrSet("data-source-line", String(token.map[0] + 1));
        token.attrSet("data-source-end", String(token.map[1]));
      }
    });
    const fence = md.renderer.rules.fence.bind(md.renderer.rules);
    md.renderer.rules.fence = (tokens, index, options, env, self) => {
      const token = tokens[index];
      if (token.info.trim().split(/\s+/)[0] === "mermaid") {
        const line = token.map ? token.map[0] + 1 : 1;
        const end = token.map ? token.map[1] : line;
        if (token.content.length > MAX_MERMAID_CHARS) {
          return `<pre class="source-line" data-source-line="${line}" data-source-end="${end}"><code>Mermaid diagram omitted: input exceeds ${MAX_MERMAID_CHARS} characters.</code></pre>`;
        }
        return `<pre class="mermaid source-line" data-source-line="${line}" data-source-end="${end}">${md.utils.escapeHtml(token.content)}</pre>`;
      }
      const line = token.map ? token.map[0] + 1 : 1;
      const end = token.map ? token.map[1] : line;
      const anchors = Array.from(
        { length: Math.max(1, end - line + 1) },
        (_, offset) => `<i class="code-source-line" data-source-line="${line + offset}" data-source-end="${line + offset}" style="--code-line:${offset}"></i>`,
      ).join("");
      return `<div class="source-line source-fence" data-source-line="${line}" data-source-end="${end}">${anchors}${fence(tokens, index, options, env, self)}</div>`;
    };
  }

  const md = window.markdownit({
    html: false,
    linkify: true,
    typographer: true,
    breaks: false,
    highlight(code, language) {
      if (code.length > MAX_CODE_CHARS) return escapeHtml(code);
      if (language && window.hljs.getLanguage(language)) {
        try {
          return window.hljs.highlight(language, code, true).value;
        } catch (_) {}
      }
      return window.hljs.highlightAuto(code).value;
    },
  }).use(sourceLines);

  function escapeHtml(text) {
    return text.replace(/[&<>"']/g, (character) => ({
      "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
    })[character]);
  }

  function containsLine(element, line) {
    const start = Number(element.dataset.sourceLine || 0);
    const end = Number(element.dataset.sourceEnd || start);
    return line >= start && line <= end;
  }

  function closestBlock(root, line) {
    const blocks = [...root.querySelectorAll("[data-source-line]")];
    const containing = blocks
      .filter((block) => containsLine(block, line))
      .sort((left, right) => {
        const leftSpan = Number(left.dataset.sourceEnd) - Number(left.dataset.sourceLine);
        const rightSpan = Number(right.dataset.sourceEnd) - Number(right.dataset.sourceLine);
        return leftSpan - rightSpan;
      });
    if (containing.length) return containing[0];
    let best = null;
    for (const block of blocks) {
      if (Number(block.dataset.sourceLine) > line) break;
      best = block;
    }
    return best || blocks[0] || null;
  }

  function decorate(root, changedLines, className, notes) {
    for (const line of changedLines) closestBlock(root, line)?.classList.add(className);
    for (const note of notes) {
      const anchor = closestBlock(root, note.line_end);
      if (!anchor) continue;
      const badge = document.createElement("aside");
      badge.className = "review-note";
      badge.textContent = `${note.kind} · lines ${note.line_start}-${note.line_end} · ${note.text}`;
      anchor.insertAdjacentElement("afterend", badge);
    }
  }

  function renderRevision(root, markdown, lines, className, notes) {
    root.innerHTML = markdown ? md.render(markdown) : '<p class="empty-revision">This revision has no content.</p>';
    decorate(root, lines, className, notes);
  }

  function renderMermaid() {
    if (!window.mermaid) return;
    try {
      window.mermaid.initialize({ startOnLoad: false, theme: "dark", securityLevel: "strict" });
      window.mermaid.init(undefined, document.querySelectorAll(".mermaid"));
    } catch (error) {
      status.textContent = `Mermaid error · ${error.message}`;
    }
  }

  function scrollToFocus(payload) {
    document.querySelectorAll(".source-focus").forEach((node) => node.classList.remove("source-focus"));
    const preferred = payload.focus_side === "old" ? previous : current;
    const fallback = preferred === current ? previous : current;
    const target = closestBlock(preferred, payload.focus_line) || closestBlock(fallback, payload.focus_line);
    if (!target) return;
    target.classList.add("source-focus");
    const toolbarHeight = 84;
    const top = target.getBoundingClientRect().top + window.scrollY - toolbarHeight;
    window.scrollTo({ top: Math.max(0, top - window.innerHeight * 0.28), behavior: "auto" });
  }

  async function fetchJson(path) {
    const response = await fetch(`${path}?token=${encodeURIComponent(previewToken)}`, { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  }

  async function refresh() {
    if (refreshing) return;
    refreshing = true;
    try {
      const focus = await fetchJson("/focus.json");
      const nextFocus = `${focus.revision}:${focus.focus_side}:${focus.focus_line}`;
      if (contentRevision !== focus.revision) {
        const document = await fetchJson("/document.json");
        contentRevision = document.revision;
        title.textContent = document.path;
        renderRevision(current, document.current, document.additions, "source-added", document.notes.filter((note) => note.side === "new"));
        renderRevision(previous, document.previous, document.deletions, "source-removed", document.notes.filter((note) => note.side === "old"));
        renderMermaid();
      }
      if (focusKey !== nextFocus) {
        focusKey = nextFocus;
        scrollToFocus(focus);
      }
      status.textContent = `${focus.focus_side} · line ${focus.focus_line} · live`;
      await fetchJson("/ready.json");
    } catch (error) {
      status.textContent = `reconnecting · ${error.message}`;
    } finally {
      refreshing = false;
    }
  }

  refresh();
  window.setInterval(refresh, 120);
})();
