// fast-grep — interactive site demos.
//
// Two demos live here:
//   1. Pattern → trigram decomposition (how the index decides which posting
//      lists to read for a given query).
//   2. Animated pipeline showing the 5-stage indexed search path.
//
// The decomposition is a *faithful simplification* of the real Rust algorithm
// in src/searcher.rs (extract_longest_literal, try_literal_alternation) and
// src/sparse.rs (sliding-window n-gram extraction). It correctly identifies
// regex metachars and alternations, but uses fixed-size 3-grams instead of
// the corpus-adaptive sparse n-grams the real engine uses — that variant
// needs a precomputed bigram frequency table that's too large to ship in JS.

(() => {
  const REGEX_META = new Set([
    '.','*','+','?','[',']','(',')','{','}','|','^','$','\\'
  ]);

  /** @returns {{type: 'literal' | 'alternation' | 'regex', parts: string[]}} */
  function classifyPattern(pat) {
    if (!pat) return { type: 'literal', parts: [] };
    // Unescaped top-level | with no other regex metachars in branches → alternation
    if (pat.includes('|') && !pat.startsWith('(?')) {
      const branches = splitTopLevelAlternation(pat);
      const allLit = branches.every(b => [...b].every(c => !REGEX_META.has(c)));
      if (allLit && branches.length > 1) {
        return { type: 'alternation', parts: branches };
      }
    }
    if ([...pat].every(c => !REGEX_META.has(c))) {
      return { type: 'literal', parts: [pat] };
    }
    return { type: 'regex', parts: extractLiteralRuns(pat) };
  }

  function splitTopLevelAlternation(pat) {
    const out = [];
    let depth = 0;
    let last = 0;
    for (let i = 0; i < pat.length; i++) {
      const c = pat[i];
      if (c === '\\') { i++; continue; }
      if (c === '(' || c === '[' || c === '{') depth++;
      else if (c === ')' || c === ']' || c === '}') depth--;
      else if (c === '|' && depth === 0) {
        out.push(pat.slice(last, i));
        last = i + 1;
      }
    }
    out.push(pat.slice(last));
    return out.map(s => s.trim()).filter(Boolean);
  }

  function extractLiteralRuns(pat) {
    const runs = [];
    let buf = '';
    for (let i = 0; i < pat.length; i++) {
      const c = pat[i];
      if (c === '\\' && i + 1 < pat.length) {
        const next = pat[i + 1];
        if (REGEX_META.has(next)) { buf += next; i++; continue; }
        if (buf) { runs.push(buf); buf = ''; }
        i++;
        continue;
      }
      if (REGEX_META.has(c)) {
        if (buf) { runs.push(buf); buf = ''; }
        // skip char class / group / quantifier wholesale
        if (c === '[') { while (i < pat.length && pat[i] !== ']') i++; }
        else if (c === '(') { while (i < pat.length && pat[i] !== ')') i++; }
        else if (c === '{') { while (i < pat.length && pat[i] !== '}') i++; }
      } else {
        buf += c;
      }
    }
    if (buf) runs.push(buf);
    return runs.filter(r => r.length >= 3);
  }

  function trigramsOf(s) {
    if (s.length < 3) return [];
    const out = [];
    for (let i = 0; i <= s.length - 3; i++) out.push(s.slice(i, i + 3));
    return out;
  }

  // -------- Demo 1: pattern → trigram decomposition --------
  const $input = document.querySelector('#pat-input');
  const $output = document.querySelector('#pat-output');
  const $presets = document.querySelectorAll('.preset-btn');

  function renderDecomposition() {
    const pat = $input.value;
    const cls = classifyPattern(pat);
    const allTrigrams = new Set();
    cls.parts.forEach(p => trigramsOf(p).forEach(t => allTrigrams.add(t)));

    const verdict = cls.type === 'literal'
      ? `Literal pattern. The index reads ${allTrigrams.size} posting list${allTrigrams.size === 1 ? '' : 's'} and intersects them; only files that contain ALL of these trigrams (in the right adjacency, per position masks) survive to the verify stage.`
      : cls.type === 'alternation'
      ? `Pure literal alternation (${cls.parts.length} branches). The index reads each branch's trigrams and unions the results — a file matches if it contains ALL trigrams of AT LEAST ONE branch.`
      : `Regex with metacharacters. We extract literal runs of ≥3 chars (${cls.parts.length} found) and use their trigrams as a pre-filter. Final regex matching happens at verify time on the candidate files.`;

    let html = '';
    html += `<div class="output-section"><h5>Pattern type</h5><div class="note">${escapeHtml(verdict)}</div></div>`;
    if (cls.parts.length === 0) {
      html += `<div class="output-section"><h5>Trigrams</h5><div class="note">No literal run of length ≥3 found — the index can't pre-filter this pattern. fast-grep falls back to a full parallel scan.</div></div>`;
    } else {
      cls.parts.forEach(part => {
        const tris = trigramsOf(part);
        html += `<div class="output-section"><h5>${cls.type === 'regex' ? 'Literal run' : 'Branch'}: <code>${escapeHtml(part)}</code></h5>`;
        if (tris.length === 0) {
          html += `<div class="note">Too short for trigrams (need ≥3 chars).</div>`;
        } else {
          tris.forEach(t => { html += `<span class="trigram${cls.type === 'regex' ? ' literal-only' : ''}">${escapeHtml(t)}</span>`; });
        }
        html += `</div>`;
      });
    }
    $output.innerHTML = html;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
  }

  if ($input) {
    $input.addEventListener('input', renderDecomposition);
    $presets.forEach(btn => btn.addEventListener('click', () => {
      $input.value = btn.dataset.pattern;
      renderDecomposition();
      $input.focus();
    }));
    renderDecomposition();
  }

  // -------- Demo 2: animated 5-stage pipeline --------
  const $steps = document.querySelectorAll('.pipeline-step');
  const $stepLabel = document.querySelector('.pipeline-controls .step-label');
  const $playBtn = document.querySelector('.pipeline-controls .play');
  const $resetBtn = document.querySelector('.pipeline-controls .reset');

  const stagePayloads = [
    { idx: 0, name: 'Pattern',       value: '"EXPORT_SYMBOL"' },
    { idx: 1, name: 'Trigrams',      value: 'EXP, XPO, POR, ORT, RT_, T_S, _SY, SYM, YMB, MBO, BOL' },
    { idx: 2, name: 'Posting lists', value: '11 lookups → ~340 candidate (file, line) pairs' },
    { idx: 3, name: 'Intersection',  value: '~340 → ~210 after position-mask Bloom filter' },
    { idx: 4, name: 'Verify',        value: '4-byte prefix + regex → 197 matches in 0.3 ms' },
  ];

  let cur = -1;
  let timer = null;

  function clearAll() {
    $steps.forEach(s => {
      s.classList.remove('active');
      s.querySelector('.value').textContent = '';
    });
    if ($stepLabel) $stepLabel.textContent = 'Press play to start';
  }

  function activate(i) {
    $steps.forEach(s => s.classList.remove('active'));
    if (i < 0 || i >= stagePayloads.length) return;
    const stage = stagePayloads[i];
    $steps[i].classList.add('active');
    $steps[i].querySelector('.value').textContent = stage.value;
    if ($stepLabel) $stepLabel.textContent = `Step ${i + 1} / ${stagePayloads.length}: ${stage.name}`;
  }

  function play() {
    if (timer) return;
    if (cur >= stagePayloads.length - 1) {
      clearAll();
      cur = -1;
    }
    timer = setInterval(() => {
      cur += 1;
      if (cur >= stagePayloads.length) {
        clearInterval(timer);
        timer = null;
        return;
      }
      activate(cur);
    }, 900);
  }

  function reset() {
    if (timer) { clearInterval(timer); timer = null; }
    cur = -1;
    clearAll();
  }

  if ($playBtn) $playBtn.addEventListener('click', play);
  if ($resetBtn) $resetBtn.addEventListener('click', reset);
  if ($steps.length) clearAll();
})();
