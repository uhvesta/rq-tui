/* rev fork: static Markdown-It renderer + markdown-preview.nvim-style source sync. */
(() => {
  "use strict";

  const richDiff = document.querySelector("#rich-diff");
  const title = document.querySelector("#review-title");
  const status = document.querySelector("#review-status");
  let contentRevision = null;
  let focusKey = null;
  let refreshing = false;
  const previewToken = document.body.dataset.previewToken;
  const MAX_CODE_CHARS = 200000;
  const MAX_MERMAID_CHARS = 100000;
  const MAX_LCS_BLOCKS = 600;

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

  function blockRange(block) {
    const mapped = block.matches("[data-source-line]")
      ? [block]
      : [...block.querySelectorAll("[data-source-line]")];
    if (!mapped.length) return null;
    return {
      start: Math.min(...mapped.map((node) => Number(node.dataset.sourceLine))),
      end: Math.max(...mapped.map((node) => Number(node.dataset.sourceEnd || node.dataset.sourceLine))),
    };
  }

  function fingerprint(block) {
    return `${block.tagName}\0${block.textContent.replace(/\s+/g, " ").trim()}`;
  }

  function renderBlocks(markdown) {
    const root = document.createElement("article");
    root.innerHTML = markdown ? md.render(markdown) : "";
    return [...root.children].map((block) => ({
      block,
      fingerprint: fingerprint(block),
      range: blockRange(block),
    }));
  }

  function largeDiffBlocks(previous, current) {
    const positions = (blocks) => {
      const result = new Map();
      blocks.forEach((entry, index) => {
        const matches = result.get(entry.fingerprint) || [];
        matches.push(index);
        result.set(entry.fingerprint, matches);
      });
      return result;
    };
    const oldPositions = positions(previous);
    const newPositions = positions(current);
    const candidates = [];
    oldPositions.forEach((oldMatches, fingerprint) => {
      const newMatches = newPositions.get(fingerprint);
      if (oldMatches.length === 1 && newMatches?.length === 1) {
        candidates.push({ oldIndex: oldMatches[0], newIndex: newMatches[0] });
      }
    });
    candidates.sort((left, right) => left.oldIndex - right.oldIndex);

    const tails = [];
    const predecessors = new Array(candidates.length).fill(-1);
    for (let index = 0; index < candidates.length; index += 1) {
      let low = 0;
      let high = tails.length;
      while (low < high) {
        const middle = (low + high) >> 1;
        if (candidates[tails[middle]].newIndex < candidates[index].newIndex) low = middle + 1;
        else high = middle;
      }
      if (low > 0) predecessors[index] = tails[low - 1];
      tails[low] = index;
    }
    const anchors = [];
    for (let index = tails[tails.length - 1]; index !== undefined && index >= 0;
         index = predecessors[index]) {
      anchors.push(candidates[index]);
    }
    anchors.reverse();

    const merged = [];
    let oldIndex = 0;
    let newIndex = 0;
    const emitChangedRange = (oldEnd, newEnd) => {
      while (oldIndex < oldEnd || newIndex < newEnd) {
        if (oldIndex < oldEnd && newIndex < newEnd &&
            previous[oldIndex].fingerprint === current[newIndex].fingerprint) {
          merged.push({ entry: current[newIndex], side: "new", changed: false });
          oldIndex += 1;
          newIndex += 1;
          continue;
        }
        if (oldIndex < oldEnd) {
          merged.push({ entry: previous[oldIndex], side: "old", changed: true });
          oldIndex += 1;
        }
        if (newIndex < newEnd) {
          merged.push({ entry: current[newIndex], side: "new", changed: true });
          newIndex += 1;
        }
      }
    };
    for (const anchor of anchors) {
      emitChangedRange(anchor.oldIndex, anchor.newIndex);
      merged.push({ entry: current[anchor.newIndex], side: "new", changed: false });
      oldIndex = anchor.oldIndex + 1;
      newIndex = anchor.newIndex + 1;
    }
    emitChangedRange(previous.length, current.length);
    return merged;
  }

  function diffBlocks(previous, current) {
    if (previous.length > MAX_LCS_BLOCKS || current.length > MAX_LCS_BLOCKS) {
      return largeDiffBlocks(previous, current);
    }
    const rows = previous.length + 1;
    const columns = current.length + 1;
    const lcs = Array.from({ length: rows }, () => new Uint16Array(columns));
    for (let oldIndex = previous.length - 1; oldIndex >= 0; oldIndex -= 1) {
      for (let newIndex = current.length - 1; newIndex >= 0; newIndex -= 1) {
        lcs[oldIndex][newIndex] = previous[oldIndex].fingerprint === current[newIndex].fingerprint
          ? lcs[oldIndex + 1][newIndex + 1] + 1
          : Math.max(lcs[oldIndex + 1][newIndex], lcs[oldIndex][newIndex + 1]);
      }
    }
    const merged = [];
    let oldIndex = 0;
    let newIndex = 0;
    while (oldIndex < previous.length || newIndex < current.length) {
      if (oldIndex < previous.length && newIndex < current.length &&
          previous[oldIndex].fingerprint === current[newIndex].fingerprint) {
        merged.push({ entry: current[newIndex], side: "new", changed: false });
        oldIndex += 1;
        newIndex += 1;
      } else if (oldIndex < previous.length &&
                 (newIndex === current.length ||
                  lcs[oldIndex + 1][newIndex] >= lcs[oldIndex][newIndex + 1])) {
        merged.push({ entry: previous[oldIndex], side: "old", changed: true });
        oldIndex += 1;
      } else {
        merged.push({ entry: current[newIndex], side: "new", changed: true });
        newIndex += 1;
      }
    }
    const interleaved = [];
    for (let index = 0; index < merged.length;) {
      if (!merged[index].changed) {
        interleaved.push(merged[index]);
        index += 1;
        continue;
      }
      const run = [];
      while (index < merged.length && merged[index].changed) {
        run.push(merged[index]);
        index += 1;
      }
      const removed = run.filter((item) => item.side === "old");
      const added = run.filter((item) => item.side === "new");
      for (let pair = 0; pair < Math.max(removed.length, added.length); pair += 1) {
        if (removed[pair]) interleaved.push(removed[pair]);
        if (added[pair]) interleaved.push(added[pair]);
      }
    }
    return interleaved;
  }

  window.__revPreviewInspect = Object.freeze({
    diffFingerprints(previousFingerprints, currentFingerprints) {
      const entries = (fingerprints) =>
        fingerprints.map((fingerprint) => ({ block: null, fingerprint, range: null }));
      return diffBlocks(entries(previousFingerprints), entries(currentFingerprints))
        .map((item) => ({
          fingerprint: item.entry.fingerprint,
          side: item.side,
          changed: item.changed,
        }));
    },
  });

  function runFixtureChecks() {
    const previous = Array.from({ length: 605 }, (_, index) => `block-${index}`);
    const current = [...previous.slice(0, 3), "inserted", ...previous.slice(3)];
    const changed = window.__revPreviewInspect
      .diffFingerprints(previous, current)
      .filter((item) => item.changed);
    document.body.dataset.largeDiffCheck =
      changed.length === 1 && changed[0].fingerprint === "inserted" &&
      changed[0].side === "new" ? "passed" : "failed";
  }

  function intersects(range, changedLines) {
    return range && changedLines.some((line) => line >= range.start && line <= range.end);
  }

  function renderRichDiff(payload) {
    const previous = renderBlocks(payload.previous);
    const current = renderBlocks(payload.current);
    const fragment = document.createDocumentFragment();
    for (const item of diffBlocks(previous, current)) {
      const node = item.entry.block;
      const changedLines = item.side === "old" ? payload.deletions : payload.additions;
      const changed = item.changed || intersects(item.entry.range, changedLines);
      node.dataset.diffSide = item.side;
      if (item.entry.range) {
        node.dataset.sourceLine = String(item.entry.range.start);
        node.dataset.sourceEnd = String(item.entry.range.end);
      }
      if (changed) {
        node.classList.add("diff-block", item.side === "old" ? "source-removed" : "source-added");
      }
      fragment.appendChild(node);
    }
    richDiff.replaceChildren(fragment);
    for (const note of payload.notes) {
      const candidates = [...richDiff.querySelectorAll(`[data-diff-side="${note.side}"]`)];
      const anchor = candidates
        .filter((node) => containsLine(node, note.line_end))
        .sort((left, right) =>
          (Number(left.dataset.sourceEnd) - Number(left.dataset.sourceLine)) -
          (Number(right.dataset.sourceEnd) - Number(right.dataset.sourceLine)))[0];
      if (!anchor) continue;
      const badge = document.createElement("aside");
      badge.className = "review-note";
      badge.textContent = `${note.kind} · lines ${note.line_start}-${note.line_end} · ${note.text}`;
      anchor.insertAdjacentElement("afterend", badge);
    }
    if (!richDiff.children.length) {
      richDiff.innerHTML = '<p class="empty-revision">This revision has no rendered content.</p>';
    }
  }

  async function renderMermaid() {
    if (!window.mermaid) return;
    window.mermaid.initialize({ startOnLoad: false, theme: "dark", securityLevel: "strict" });
    const nodes = document.querySelectorAll(".mermaid");
    if (window.mermaid.run) {
      await window.mermaid.run({ nodes, suppressErrors: false });
    } else {
      await Promise.resolve(window.mermaid.init(undefined, nodes));
    }
  }

  // Ported from markdown-preview.nvim's `upstream-source/scroll.js` relative-scroll
  // model (smooth, viewport-position-preserving), rewritten without GSAP/TweenLite
  // (not vendored) and adapted to use our block start/end line ranges - which give
  // us a source line's fractional position *inside* its block - instead of upstream's
  // single-line-per-block interpolation between neighboring anchors.
  const TOOLBAR_HEIGHT = 84;
  const FOCUS_VIEWPORT_RATIO = 0.28;
  const SCROLL_DURATION_MS = 220;
  let scrollAnimationFrame = null;
  let hasScrolledOnce = false;
  let viewportRevision = null;

  function easeOutCubic(t) {
    return 1 - (1 - t) ** 3;
  }

  function animateScrollTo(target, animate) {
    if (scrollAnimationFrame !== null) {
      cancelAnimationFrame(scrollAnimationFrame);
      scrollAnimationFrame = null;
    }
    const from = window.scrollY;
    const delta = target - from;
    if (!animate || Math.abs(delta) < 1) {
      window.scrollTo(0, Math.max(0, target));
      return;
    }
    const start = performance.now();
    const step = (now) => {
      const progress = Math.min(1, (now - start) / SCROLL_DURATION_MS);
      window.scrollTo(0, Math.max(0, from + delta * easeOutCubic(progress)));
      scrollAnimationFrame = progress < 1 ? requestAnimationFrame(step) : null;
    };
    scrollAnimationFrame = requestAnimationFrame(step);
  }

  function blockOffsetRatio(block, line) {
    const start = Number(block.dataset.sourceLine || line);
    const end = Number(block.dataset.sourceEnd || start);
    if (end <= start) return 0;
    return Math.min(1, Math.max(0, (line - start) / (end - start)));
  }

  function scrollToFocus(payload) {
    document.querySelectorAll(".source-focus").forEach((node) => node.classList.remove("source-focus"));
    const mapped = [...richDiff.querySelectorAll("[data-source-line]")];
    const containing = mapped
      .filter((node) => containsLine(node, payload.focus_line))
      .sort((left, right) =>
        (Number(left.dataset.sourceEnd) - Number(left.dataset.sourceLine)) -
        (Number(right.dataset.sourceEnd) - Number(right.dataset.sourceLine)));
    const target = containing.find((node) =>
      node.closest(`[data-diff-side="${payload.focus_side}"]`)) ||
      containing[0] ||
      closestBlock(richDiff, payload.focus_line);
    if (!target) return;
    target.classList.add("source-focus");
    const rect = target.getBoundingClientRect();
    const outsideViewport = rect.top < TOOLBAR_HEIGHT || rect.bottom > window.innerHeight;
    const nextViewportRevision = `${payload.revision}:${payload.viewport_top}:${payload.cursor_fraction}`;
    const viewportMoved = viewportRevision !== nextViewportRevision;
    viewportRevision = nextViewportRevision;
    if (!viewportMoved && !outsideViewport) return;
    const ratio = blockOffsetRatio(target, payload.focus_line);
    const anchor = rect.top + window.scrollY + rect.height * ratio - TOOLBAR_HEIGHT;
    const cursorRatio = Number.isFinite(payload.cursor_fraction)
      ? Math.max(0, Math.min(1, payload.cursor_fraction))
      : FOCUS_VIEWPORT_RATIO;
    const usableViewportHeight = Math.max(1, window.innerHeight - TOOLBAR_HEIGHT);
    const desiredFocusY = TOOLBAR_HEIGHT + usableViewportHeight * cursorRatio;
    const rawTop = anchor + TOOLBAR_HEIGHT - desiredFocusY;
    const blockTop = rect.top + window.scrollY;
    const minimumTop = blockTop + rect.height - window.innerHeight;
    const maximumTop = blockTop - TOOLBAR_HEIGHT;
    const top = rect.height <= usableViewportHeight
      ? Math.max(minimumTop, Math.min(maximumTop, rawTop))
      : rawTop;
    animateScrollTo(top, hasScrolledOnce);
    hasScrolledOnce = true;
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
      // Fire the focus poll and the liveness heartbeat together instead of in
      // series - ready.json doesn't depend on focus.json's result, so awaiting
      // them one after another was adding a full extra round trip to every tick.
      const [focus] = await Promise.all([fetchJson("/focus.json"), fetchJson("/ready.json")]);
      const nextFocus = `${focus.revision}:${focus.focus_side}:${focus.focus_line}:${focus.viewport_top}:${focus.cursor_fraction}`;
      if (contentRevision !== focus.revision) {
        const review = await fetchJson("/document.json");
        if (review.revision !== focus.revision) return;
        contentRevision = review.revision;
        title.textContent = review.path;
        renderRichDiff(review);
        await renderMermaid();
        if (review.browser_fixture) runFixtureChecks();
      }
      if (focusKey !== nextFocus) {
        focusKey = nextFocus;
        scrollToFocus(focus);
      }
      status.textContent = `${focus.focus_side} · line ${focus.focus_line} · live`;
    } catch (error) {
      status.textContent = `reconnecting · ${error.message}`;
    } finally {
      refreshing = false;
    }
  }

  refresh();
  window.setInterval(refresh, 120);
})();
