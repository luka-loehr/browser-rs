// Page-side runtime. Runs in an isolated world ("__bmcp") of every document, so page scripts can
// neither see nor clobber it, while it shares the page's DOM. Produces the accessibility snapshot,
// owns element refs, and does the element-level checks the Rust side turns into trusted input.
(() => {
  if (globalThis.__bmcp) return;

  const refs = new Map();       // "e12" -> WeakRef<Element>
  const refOf = new WeakMap();  // Element -> "e12"
  let counter = 0;
  let lastSnapshotRoot = null;

  function refFor(el) {
    let r = refOf.get(el);
    if (!r) { r = 'e' + (++counter); refOf.set(el, r); }
    refs.set(r, new WeakRef(el));
    return r;
  }

  // ---------------------------------------------------------------- roles & names

  const INPUT_ROLES = {
    button: 'button', submit: 'button', reset: 'button', image: 'button',
    checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton',
    search: 'searchbox', email: 'textbox', tel: 'textbox', text: 'textbox', url: 'textbox',
    password: 'textbox', file: 'button', color: 'button',
    date: 'textbox', 'datetime-local': 'textbox', month: 'textbox', time: 'textbox', week: 'textbox',
  };
  const TAG_ROLES = {
    ARTICLE: 'article', ASIDE: 'complementary', BLOCKQUOTE: 'blockquote', BUTTON: 'button',
    CAPTION: 'caption', CODE: 'code', DD: 'definition', DEL: 'deletion', DETAILS: 'group',
    DIALOG: 'dialog', DT: 'term', EM: 'emphasis', FIELDSET: 'group', FIGURE: 'figure',
    H1: 'heading', H2: 'heading', H3: 'heading', H4: 'heading', H5: 'heading', H6: 'heading',
    HR: 'separator', IFRAME: 'iframe', INS: 'insertion', LI: 'listitem', MAIN: 'main', MARK: 'mark',
    MATH: 'math', MENU: 'list', METER: 'meter', NAV: 'navigation', OL: 'list', OPTGROUP: 'group',
    OPTION: 'option', OUTPUT: 'status', P: 'paragraph', PRE: 'generic', PROGRESS: 'progressbar',
    STRONG: 'strong', SUB: 'subscript', SUP: 'superscript', SUMMARY: 'button', SVG: 'img',
    TABLE: 'table', TBODY: 'rowgroup', TEXTAREA: 'textbox', TFOOT: 'rowgroup', THEAD: 'rowgroup',
    TIME: 'time', TR: 'row', UL: 'list', LEGEND: 'legend', LABEL: null,
  };
  const NAME_FROM_CONTENT = new Set([
    // "row" is left out on purpose: a row named after its content repeats every cell's text.
    'button', 'cell', 'checkbox', 'columnheader', 'gridcell', 'heading', 'link', 'menuitem',
    'menuitemcheckbox', 'menuitemradio', 'option', 'radio', 'rowheader', 'switch', 'tab',
    'tooltip', 'treeitem', 'legend', 'caption', 'term',
  ]);
  const LANDMARK_NEEDS_NAME = new Set(['form', 'region']);

  function hasAncestor(el, tags) {
    for (let p = el.parentElement; p; p = p.parentElement) if (tags.includes(p.tagName)) return true;
    return false;
  }

  function roleOf(el) {
    const explicit = (el.getAttribute('role') || '').trim().split(/\s+/)[0];
    if (explicit && explicit !== 'none' && explicit !== 'presentation') return explicit;
    if (explicit === 'none' || explicit === 'presentation') return null;
    const tag = el.tagName;
    switch (tag) {
      case 'A': case 'AREA': return el.hasAttribute('href') ? 'link' : null;
      case 'INPUT': {
        const type = (el.getAttribute('type') || 'text').toLowerCase();
        if (type === 'hidden') return null;
        if (el.hasAttribute('list') && ['text', 'search', 'email', 'tel', 'url'].includes(type)) return 'combobox';
        return INPUT_ROLES[type] || 'textbox';
      }
      case 'SELECT': return el.multiple || el.size > 1 ? 'listbox' : 'combobox';
      case 'IMG': return el.getAttribute('alt') === '' && !attrOf(el, 'title') ? null : 'img';
      case 'HEADER': return hasAncestor(el, ['ARTICLE', 'ASIDE', 'MAIN', 'NAV', 'SECTION']) ? null : 'banner';
      case 'FOOTER': return hasAncestor(el, ['ARTICLE', 'ASIDE', 'MAIN', 'NAV', 'SECTION']) ? null : 'contentinfo';
      case 'SECTION': return 'region';
      case 'FORM': return 'form';
      case 'TD': return hasAncestor(el, ['TABLE']) ? 'cell' : null;
      case 'TH': return el.scope === 'row' ? 'rowheader' : 'columnheader';
      case 'DL': return 'list';
    }
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return 'textbox';
    return TAG_ROLES[tag] || null;
  }

  // Properties like form.title, form.id or form.hidden can be shadowed by form controls named
  // "title", "id", "hidden"; so this runtime reads attributes and never trusts such a property.
  const collapse = s => (typeof s === 'string' ? s : '').replace(/\s+/g, ' ').trim();
  const attrOf = (el, n) => el.getAttribute(n) || '';
  const isDisabled = el => { try { return el.matches(':disabled'); } catch { return false; } };

  function hiddenForAria(el) {
    if (el.hasAttribute('hidden') || el.getAttribute('aria-hidden') === 'true' || el.hasAttribute('inert')) return true;
    if (['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD', 'META', 'LINK'].includes(el.tagName)) return true;
    const style = getComputedStyle(el);
    if (style.display === 'none') return true;
    // visibility is inherited and can be overridden by descendants, so it only hides this node's own box
    return false;
  }

  function isVisible(el) {
    if (!el.checkVisibility({ visibilityProperty: true, opacityProperty: false })) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  }

  function textFromContent(el, depth = 0) {
    if (depth > 20) return '';
    let out = '';
    const kids = el.shadowRoot ? el.shadowRoot.childNodes : el.childNodes;
    for (const n of kids) {
      if (n.nodeType === Node.TEXT_NODE) out += n.data;
      else if (n.nodeType === Node.ELEMENT_NODE) {
        if (n.tagName === 'SLOT') { for (const a of n.assignedNodes({ flatten: true })) out += a.nodeType === 3 ? a.data : textFromContent(a, depth + 1); continue; }
        if (hiddenForAria(n)) continue;
        const label = n.getAttribute('aria-label');
        if (label) { out += ' ' + label + ' '; continue; }
        if (n.tagName === 'IMG') { out += ' ' + (n.getAttribute('alt') || '') + ' '; continue; }
        const block = getComputedStyle(n).display;
        const sep = block && !block.startsWith('inline') ? ' ' : '';
        out += sep + textFromContent(n, depth + 1) + sep;
      }
    }
    return out;
  }

  function labelsText(el) {
    if (!el.labels || !el.labels.length) return '';
    return collapse([...el.labels].map(l => textFromContent(l)).join(' '));
  }

  function nameOf(el, role) {
    const by = el.getAttribute('aria-labelledby');
    if (by) {
      const t = collapse(by.split(/\s+/).map(id => { const n = el.ownerDocument.getElementById(id); return n ? textFromContent(n) : ''; }).join(' '));
      if (t) return t;
    }
    const label = collapse(el.getAttribute('aria-label'));
    if (label) return label;
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') {
      const type = (el.getAttribute('type') || '').toLowerCase();
      if (['button', 'submit', 'reset'].includes(type)) return collapse(el.value) || (type === 'submit' ? 'Submit' : type === 'reset' ? 'Reset' : '');
      if (type === 'image') return collapse(el.alt) || 'Submit';
      const l = labelsText(el);
      if (l) return l;
      return collapse(attrOf(el, 'placeholder') || attrOf(el, 'title'));
    }
    if (tag === 'IMG' || tag === 'AREA') return collapse(attrOf(el, 'alt') || attrOf(el, 'title'));
    if (tag === 'SVG' || tag === 'svg') { const t = el.querySelector('title'); return collapse(t ? t.textContent : ''); }
    if (tag === 'FIELDSET') { const l = el.querySelector(':scope > legend'); if (l) return collapse(textFromContent(l)); }
    if (tag === 'TABLE') { const c = el.querySelector(':scope > caption'); if (c) return collapse(textFromContent(c)); }
    if (tag === 'FIGURE') { const c = el.querySelector(':scope > figcaption'); if (c) return collapse(textFromContent(c)); }
    if (role && NAME_FROM_CONTENT.has(role)) {
      const t = collapse(textFromContent(el));
      if (t) return t.length > 150 ? t.slice(0, 150) + '…' : t;
    }
    return collapse(attrOf(el, 'title'));
  }

  function ariaProps(el, role) {
    const p = [];
    const attr = n => el.getAttribute(n);
    if (role === 'heading') p.push(`level=${attr('aria-level') || (/^H[1-6]$/.test(el.tagName) ? el.tagName[1] : 2)}`);
    if (role === 'checkbox' || role === 'radio' || role === 'switch' || role === 'menuitemcheckbox' || role === 'menuitemradio') {
      const checked = el.tagName === 'INPUT' ? (el.indeterminate ? 'mixed' : el.checked) : attr('aria-checked');
      if (checked === 'mixed') p.push('checked=mixed'); else if (checked === true || checked === 'true') p.push('checked');
    }
    if (attr('aria-pressed') === 'true') p.push('pressed');
    if (attr('aria-pressed') === 'mixed') p.push('pressed=mixed');
    const expanded = el.tagName === 'DETAILS' ? String(el.open) : attr('aria-expanded');
    if (expanded === 'true') p.push('expanded');
    if (el.tagName === 'OPTION' ? el.selected : attr('aria-selected') === 'true') p.push('selected');
    if (isDisabled(el) || attr('aria-disabled') === 'true') p.push('disabled');
    if (el === deepActiveElement(el.ownerDocument) && role !== 'document') p.push('active');
    return p;
  }

  function deepActiveElement(doc) {
    let a = doc.activeElement;
    while (a && a.shadowRoot && a.shadowRoot.activeElement) a = a.shadowRoot.activeElement;
    return a;
  }

  function valueOf(el, role) {
    if (el.tagName === 'SELECT') return [...el.selectedOptions].map(o => collapse(o.label)).join(', ');
    if (el.tagName === 'INPUT') {
      const type = (el.type || '').toLowerCase();
      if (['checkbox', 'radio', 'button', 'submit', 'reset', 'image', 'file', 'hidden'].includes(type)) return '';
      return type === 'password' && el.value ? '•'.repeat(Math.min(el.value.length, 12)) : el.value;
    }
    if (el.tagName === 'TEXTAREA') return el.value;
    if (role === 'textbox' && el.isContentEditable) return collapse(el.innerText);
    if (role === 'slider' || role === 'progressbar' || role === 'meter' || role === 'spinbutton') return el.getAttribute('aria-valuenow') || el.value || '';
    return '';
  }

  // ---------------------------------------------------------------- snapshot

  function childNodesOf(node) {
    if (node.nodeType === Node.ELEMENT_NODE) {
      if (node.tagName === 'SLOT') return node.assignedNodes({ flatten: true });
      if (node.shadowRoot) return node.shadowRoot.childNodes;
      if (node.tagName === 'IFRAME') {
        try { const d = node.contentDocument; return d && d.body ? [d.body] : []; } catch { return []; }
      }
    }
    return node.childNodes;
  }

  // Builds {role, name, props, value, ref, url, children: (node|string)[]}. Role-less elements are
  // flattened into their parent, so the tree carries only what an accessibility client would show.
  function build(el, opts, depth) {
    if (hiddenForAria(el)) return [];
    let role = roleOf(el);
    let node = null;
    if (role && LANDMARK_NEEDS_NAME.has(role) && !nameOf(el, role)) role = null;
    const cursorPointer = !role && opts.cursor && getComputedStyle(el).cursor === 'pointer' &&
      !(el.parentElement && getComputedStyle(el.parentElement).cursor === 'pointer');
    if (role || cursorPointer) {
      role = role || 'generic';
      node = { role, name: role === 'generic' ? '' : nameOf(el, role), props: ariaProps(el, role), value: valueOf(el, role), ref: refFor(el), children: [], el };
      if (cursorPointer) node.props.push('cursor=pointer');
      if (role === 'link' && el.getAttribute('href')) node.url = el.getAttribute('href');
      if (opts.boxes) { const r = el.getBoundingClientRect(); node.props.push(`box=${Math.round(r.x)},${Math.round(r.y)},${Math.round(r.width)},${Math.round(r.height)}`); }
    }
    const kids = [];
    const nextDepth = node ? depth + 1 : depth;
    const leaf = ['INPUT', 'TEXTAREA', 'IMG'].includes(el.tagName);
    if (!leaf && !(opts.depth != null && nextDepth > opts.depth)) {
      const visibleSelf = getComputedStyle(el).visibility !== 'hidden';
      for (const c of childNodesOf(el)) {
        if (c.nodeType === Node.TEXT_NODE) {
          if (visibleSelf) { const t = collapse(c.data); if (t) kids.push(t); }
        } else if (c.nodeType === Node.ELEMENT_NODE) {
          kids.push(...build(c, opts, nextDepth));
        }
      }
    }
    // Merge adjacent strings.
    const merged = [];
    for (const k of kids) {
      if (typeof k === 'string' && typeof merged[merged.length - 1] === 'string') merged[merged.length - 1] += ' ' + k;
      else merged.push(k);
    }
    if (!node) return merged;
    node.children = merged;
    // A name taken from content repeats the content; drop the duplicate text child.
    if (node.name && merged.length === 1 && typeof merged[0] === 'string' && collapse(merged[0]) === node.name) node.children = [];
    return [node];
  }

  const yamlStr = s => /^[\w .,:;!?()'@#%&+\-\/…]*$/.test(s) && !/^[-?:,\[\]{}#&*!|>'"%@`]/.test(s) && !s.endsWith(':') ? s : JSON.stringify(s);

  function render(nodes, indent, lines) {
    for (const n of nodes) {
      const pad = '  '.repeat(indent);
      if (typeof n === 'string') { lines.push(`${pad}- text: ${yamlStr(n)}`); continue; }
      let head = `${pad}- ${n.role}`;
      if (n.name) head += ' ' + JSON.stringify(n.name);
      for (const p of n.props) head += ` [${p}]`;
      head += ` [ref=${n.ref}]`;
      const inlineText = n.children.length === 1 && typeof n.children[0] === 'string' && !n.url ? n.children[0] : null;
      if (n.value) {
        lines.push(`${head}: ${yamlStr(n.value)}`);
        if (n.children.length && !inlineText) render(n.children, indent + 1, lines);
      } else if (inlineText !== null) {
        lines.push(`${head}: ${yamlStr(inlineText)}`);
      } else if (n.children.length || n.url) {
        lines.push(head + ':');
        if (n.url) lines.push(`${pad}  - /url: ${yamlStr(n.url)}`);
        render(n.children, indent + 1, lines);
      } else {
        lines.push(head);
      }
    }
    return lines;
  }

  function snapshot(opts = {}) {
    const root = opts.target ? resolve(opts.target) : document.body || document.documentElement;
    if (!root) return '';
    refs.clear();
    const tree = build(root, { depth: opts.depth, boxes: !!opts.boxes, cursor: true }, 0);
    lastSnapshotRoot = root;
    return render(tree, 0, []).join('\n');
  }

  // ---------------------------------------------------------------- resolving targets

  function deepQuery(root, selector) {
    const hit = root.querySelector(selector);
    if (hit) return hit;
    for (const el of root.querySelectorAll('*')) {
      if (el.shadowRoot) { const h = deepQuery(el.shadowRoot, selector); if (h) return h; }
    }
    return null;
  }

  function* allElements(root) {
    for (const el of root.querySelectorAll('*')) {
      yield el;
      if (el.shadowRoot) yield* allElements(el.shadowRoot);
    }
  }

  function parseRoleSelector(s) {
    const m = s.match(/^([a-z]+)(?:\[name=(?:"((?:[^"\\]|\\.)*)"|'([^']*)')(i|s)?\])?$/);
    if (!m) throw new Error(`Invalid role selector "role=${s}"`);
    return { role: m[1], name: m[2] !== undefined ? JSON.parse('"' + m[2] + '"') : m[3], exact: m[4] === 's' };
  }

  function resolve(target) {
    target = String(target).trim();
    if (/^(f\d+)?e\d+$/.test(target)) {
      const w = refs.get(target);
      const el = w && w.deref();
      if (!el || !el.isConnected) throw new Error(`Ref ${target} not found in the current page snapshot. Try capturing a new snapshot.`);
      return el;
    }
    if (target.startsWith('text=')) {
      const want = target.slice(5).replace(/^"(.*)"$/, '$1').toLowerCase();
      let best = null;
      for (const el of allElements(document)) {
        if (hiddenForAria(el) || !collapse(el.textContent).toLowerCase().includes(want)) continue;
        best = el; // keep descending: the deepest match is the most specific element
      }
      if (!best) throw new Error(`No element with text "${want}"`);
      return best;
    }
    if (target.startsWith('role=')) {
      const { role, name, exact } = parseRoleSelector(target.slice(5));
      for (const el of allElements(document)) {
        if (roleOf(el) !== role || hiddenForAria(el)) continue;
        if (name === undefined) return el;
        const n = nameOf(el, role);
        if (exact ? n === name : n.toLowerCase().includes(name.toLowerCase())) return el;
      }
      throw new Error(`No element matching "${target}"`);
    }
    const css = target.startsWith('css=') ? target.slice(4) : target;
    let el;
    try { el = deepQuery(document, css); } catch (e) { throw new Error(`Invalid selector "${target}": ${e.message}`); }
    if (!el) throw new Error(`No element matches selector "${target}"`);
    return el;
  }

  function describe(el) {
    const role = roleOf(el);
    const name = role ? nameOf(el, role) : '';
    let s = '<' + el.tagName.toLowerCase();
    if (attrOf(el, 'id')) s += ` id="${attrOf(el, 'id')}"`;
    if (attrOf(el, 'class')) s += ` class="${attrOf(el, 'class').trim().slice(0, 60)}"`;
    s += '>';
    if (role) s += ` (${role}${name ? ' "' + name.slice(0, 60) + '"' : ''})`;
    return s;
  }

  // ---------------------------------------------------------------- actionability

  // requestAnimationFrame never fires in a tab that is not in front, so race it with a timer.
  const raf = () => new Promise(r => {
    let done = false;
    const f = () => { if (!done) { done = true; r(); } };
    requestAnimationFrame(f);
    setTimeout(f, 32);
  });
  const sleep = ms => new Promise(r => setTimeout(r, ms));

  function frameOffset(el) {
    let x = 0, y = 0;
    let win = el.ownerDocument.defaultView;
    while (win && win !== window && win.frameElement) {
      const r = win.frameElement.getBoundingClientRect();
      const s = getComputedStyle(win.frameElement);
      x += r.left + parseFloat(s.borderLeftWidth) + parseFloat(s.paddingLeft);
      y += r.top + parseFloat(s.borderTopWidth) + parseFloat(s.paddingTop);
      win = win.parent;
    }
    return { x, y };
  }

  function deepElementFromPoint(doc, x, y) {
    let el = doc.elementFromPoint(x, y);
    while (el && el.shadowRoot) {
      const inner = el.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === el) break;
      el = inner;
    }
    return el;
  }

  function containsComposed(parent, child) {
    for (let n = child; n; n = n.parentNode || n.host) if (n === parent) return true;
    return false;
  }

  // Waits until the element is attached, visible, stable, enabled (when required) and actually
  // receives pointer events at its center; returns top-level viewport coordinates for the click.
  async function prepare(el, { timeout = 5000, enabled = true, hitTest = true, force = false } = {}) {
    const deadline = performance.now() + timeout;
    let reason = '';
    let lastRect = null;
    for (let attempt = 0; ; attempt++) {
      if (!el.isConnected) throw new Error('Element is not attached to the DOM');
      reason = '';
      if (!force && !isVisible(el)) reason = 'element is not visible';
      else if (!force && enabled && (isDisabled(el) || el.getAttribute('aria-disabled') === 'true')) reason = 'element is disabled';
      if (!reason) {
        el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
        const r = el.getBoundingClientRect();
        const stable = force || (lastRect && lastRect.x === r.x && lastRect.y === r.y && lastRect.width === r.width && lastRect.height === r.height);
        lastRect = r;
        if (!stable) reason = 'element is not stable';
        else {
          const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
          if (hitTest && !force) {
            const hit = deepElementFromPoint(el.ownerDocument, cx, cy);
            if (hit && !containsComposed(el, hit) && !(el.tagName === 'LABEL' && containsComposed(el.control || el, hit)) &&
                !(el.labels && [...el.labels].some(l => containsComposed(l, hit)))) {
              reason = `${describe(hit)} intercepts pointer events`;
            }
          }
          if (!reason) {
            const off = frameOffset(el);
            return { x: cx + off.x, y: cy + off.y };
          }
        }
      }
      if (performance.now() > deadline) throw new Error(`Timed out after ${timeout}ms waiting for ${describe(el)}: ${reason}`);
      if (attempt < 2) await raf(); else await sleep(Math.min(100, attempt * 20));
    }
  }

  function editableKind(el) {
    if (el.tagName === 'TEXTAREA') return 'text';
    if (el.tagName === 'INPUT') {
      const t = (el.type || 'text').toLowerCase();
      if (['text', 'search', 'email', 'tel', 'url', 'password', 'number', ''].includes(t)) return 'text';
      if (['date', 'datetime-local', 'month', 'time', 'week', 'color', 'range'].includes(t)) return 'set';
      return null;
    }
    if (el.isContentEditable) return 'text';
    return null;
  }

  function nativeSet(el, value) {
    const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
    el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  }

  // Focuses the element and selects its current content so trusted Input.insertText replaces it.
  // Returns "insert" (Rust inserts the text), or "done" when the value was set directly.
  function beginFill(el, value) {
    if (el.tagName === 'LABEL' && el.control) el = el.control;
    const kind = editableKind(el);
    if (!kind) throw new Error(`${describe(el)} is not an editable element`);
    if (el.readOnly) throw new Error(`${describe(el)} is read-only`);
    el.focus();
    if (kind === 'set' || value === '') {
      if (el.isContentEditable) { el.textContent = ''; el.dispatchEvent(new Event('input', { bubbles: true })); }
      else nativeSet(el, value);
      return 'done';
    }
    if (el.isContentEditable) {
      const range = el.ownerDocument.createRange();
      range.selectNodeContents(el);
      const sel = el.ownerDocument.getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
    } else if (typeof el.select === 'function') {
      try { el.select(); } catch { el.setSelectionRange(0, el.value.length); }
    }
    return 'insert';
  }

  function focusForTyping(el) {
    if (el.tagName === 'LABEL' && el.control) el = el.control;
    if (!editableKind(el)) throw new Error(`${describe(el)} is not an editable element`);
    el.focus();
    if (typeof el.setSelectionRange === 'function' && el.value != null) {
      try { el.setSelectionRange(el.value.length, el.value.length); } catch {}
    }
  }

  function selectOptions(el, values) {
    if (el.tagName !== 'SELECT') throw new Error(`${describe(el)} is not a <select> element`);
    const opts = [...el.options];
    const picked = [];
    for (const v of values) {
      const o = opts.find(o => o.value === v) || opts.find(o => collapse(o.label) === v) || opts.find(o => collapse(o.label).toLowerCase() === String(v).toLowerCase());
      if (!o) throw new Error(`Option "${v}" not found. Available: ${opts.map(o => JSON.stringify(collapse(o.label))).join(', ')}`);
      picked.push(o);
    }
    if (!el.multiple && picked.length > 1) throw new Error('Cannot select multiple options in a single-select');
    for (const o of opts) o.selected = picked.includes(o);
    el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
    return picked.map(o => o.value);
  }

  function checkedState(el) {
    if (el.tagName === 'LABEL' && el.control) el = el.control;
    if (el.tagName === 'INPUT' && ['checkbox', 'radio'].includes(el.type)) return el.checked;
    const a = el.getAttribute('aria-checked');
    if (a === 'true' || a === 'false') return a === 'true';
    throw new Error(`${describe(el)} is not a checkbox or radio`);
  }

  // ---------------------------------------------------------------- waiting

  function pageText() {
    return collapse(document.body ? document.body.innerText : document.documentElement.textContent);
  }

  function waitForText(text, gone, timeout) {
    const check = () => pageText().includes(text) !== gone;
    if (check()) return Promise.resolve(true);
    return new Promise((resolve, reject) => {
      const obs = new MutationObserver(() => { if (check()) done(true); });
      const poll = setInterval(() => { if (check()) done(true); }, 100);
      const timer = setTimeout(() => done(false), timeout);
      function done(ok) {
        obs.disconnect(); clearInterval(poll); clearTimeout(timer);
        ok ? resolve(true) : reject(new Error(`Timed out after ${timeout}ms waiting for text "${text}" to ${gone ? 'disappear' : 'appear'}`));
      }
      obs.observe(document, { subtree: true, childList: true, characterData: true, attributes: true });
    });
  }

  // ---------------------------------------------------------------- testing helpers

  function cssEscapeAttr(v) { return v.replace(/\\/g, '\\\\').replace(/'/g, "\\'"); }

  function generateLocator(el, testIdAttr) {
    const q = s => JSON.stringify(s).replace(/^"|"$/g, "'").replace(/\\"/g, '"');
    const tid = el.getAttribute(testIdAttr);
    if (tid) return `getByTestId(${q(tid)})`;
    const role = roleOf(el);
    const name = role ? nameOf(el, role) : '';
    if (role && name) {
      const matches = [...allElements(document)].filter(e => roleOf(e) === role && nameOf(e, role) === name && !hiddenForAria(e));
      if (matches.length === 1) return `getByRole(${q(role)}, { name: ${q(name)} })`;
    }
    if (el.labels && el.labels.length) { const l = labelsText(el); if (l) return `getByLabel(${q(l)})`; }
    const ph = el.getAttribute('placeholder');
    if (ph) return `getByPlaceholder(${q(ph)})`;
    const text = collapse(el.textContent);
    if (text && text.length < 80 && !el.children.length) return `getByText(${q(text)})`;
    if (attrOf(el, 'id')) return `locator('#${CSS.escape(attrOf(el, 'id'))}')`;
    const path = [];
    for (let n = el; n && n.nodeType === 1 && n !== document.documentElement; n = n.parentElement) {
      let seg = n.tagName.toLowerCase();
      if (attrOf(n, 'id')) { path.unshift(`#${CSS.escape(attrOf(n, 'id'))}`); break; }
      const sibs = n.parentElement ? [...n.parentElement.children].filter(s => s.tagName === n.tagName) : [];
      if (sibs.length > 1) seg += `:nth-of-type(${sibs.indexOf(n) + 1})`;
      path.unshift(seg);
    }
    return `locator('${cssEscapeAttr(path.join(' > '))}')`;
  }

  function isRoleVisible(role, name) {
    for (const el of allElements(document)) {
      if (roleOf(el) === role && nameOf(el, role) === name && isVisible(el)) return true;
    }
    return false;
  }

  // ---------------------------------------------------------------- overlays

  const HOST_ATTR = 'data-bmcp-overlay';

  function highlight(el, style, key) {
    const r = el.getBoundingClientRect();
    const off = frameOffset(el);
    const box = document.createElement('div');
    box.setAttribute(HOST_ATTR, 'hl-' + key);
    box.style.cssText = `position:fixed;left:${r.left + off.x}px;top:${r.top + off.y}px;width:${r.width}px;height:${r.height}px;` +
      'outline:2px solid #ff3b7f;background:rgba(255,59,127,.15);pointer-events:none;z-index:2147483647;box-sizing:border-box;' + (style || '');
    removeHighlight(key);
    (document.body || document.documentElement).appendChild(box);
  }
  function removeHighlight(key) {
    const sel = key ? `[${HOST_ATTR}="hl-${key}"]` : `[${HOST_ATTR}^="hl-"]`;
    document.querySelectorAll(sel).forEach(n => n.remove());
  }

  // The hand-off banner shown in headed mode while a human takes over. Clicking "Done" calls the
  // __bmcpHandoff binding, which only exists in this isolated world, so the page cannot fake it.
  function showHandoff(message) {
    if (document.querySelector(`[${HOST_ATTR}="handoff"]`)) return;
    const host = document.createElement('div');
    host.setAttribute(HOST_ATTR, 'handoff');
    host.style.cssText = 'all:initial;position:fixed;left:50%;bottom:20px;transform:translateX(-50%);z-index:2147483647';
    const root = host.attachShadow({ mode: 'closed' });
    // A constructable stylesheet, not a <style> tag: pages with a strict style-src CSP block
    // inline <style> elements, which left the banner unstyled on such sites.
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(`
      :host{all:initial}
      .bar{font:14px/1.4 -apple-system,system-ui,sans-serif;display:flex;gap:14px;align-items:center;background:#111;color:#fff;
        padding:12px 14px 12px 18px;border-radius:12px;box-shadow:0 10px 30px rgba(0,0,0,.35);max-width:min(720px,calc(100vw - 40px))}
      .dot{width:9px;height:9px;border-radius:50%;background:#34c759;flex:none;animation:p 1.4s infinite}
      @keyframes p{50%{opacity:.35}}
      button{font:600 14px -apple-system,system-ui,sans-serif;background:#fff;color:#111;border:0;border-radius:8px;padding:8px 14px;cursor:pointer;flex:none}
    `);
    root.adoptedStyleSheets = [sheet];
    const bar = document.createElement('div');
    bar.className = 'bar';
    const dot = document.createElement('span');
    dot.className = 'dot';
    const msg = document.createElement('span');
    msg.className = 'msg';
    const button = document.createElement('button');
    button.textContent = 'Done — hand back to agent';
    bar.append(dot, msg, button);
    root.append(bar);
    root.querySelector('.msg').textContent = message || 'The agent handed control to you.';
    root.querySelector('button').addEventListener('click', () => {
      host.remove();
      try { globalThis.__bmcpHandoff('done'); } catch {}
    });
    (document.body || document.documentElement).appendChild(host);
  }
  function hideHandoff() {
    document.querySelectorAll(`[${HOST_ATTR}="handoff"]`).forEach(n => n.remove());
  }

  // ---------------------------------------------------------------- recorder

  let recording = false;
  function record(ev) {
    try { globalThis.__bmcpRecord(JSON.stringify(ev)); } catch {}
  }
  function onRecordClick(e) {
    const el = e.composedPath()[0];
    if (!(el instanceof Element) || el.closest(`[${HOST_ATTR}]`)) return;
    record({ action: 'click', locator: generateLocator(el.closest('a,button,input,select,textarea,[role]') || el, 'data-testid') });
  }
  function onRecordChange(e) {
    const el = e.composedPath()[0];
    if (!(el instanceof Element)) return;
    if (el.tagName === 'SELECT') record({ action: 'selectOption', locator: generateLocator(el, 'data-testid'), value: [...el.selectedOptions].map(o => o.value) });
    else if (['checkbox', 'radio'].includes(el.type)) record({ action: el.checked ? 'check' : 'uncheck', locator: generateLocator(el, 'data-testid') });
    else if (editableKind(el)) record({ action: 'fill', locator: generateLocator(el, 'data-testid'), value: el.type === 'password' ? '<password>' : el.value });
  }
  function onRecordKey(e) {
    if (['Enter', 'Escape', 'Tab'].includes(e.key)) record({ action: 'press', key: e.key });
  }
  function setRecording(on) {
    if (on === recording) return;
    recording = on;
    const m = on ? 'addEventListener' : 'removeEventListener';
    document[m]('click', onRecordClick, true);
    document[m]('change', onRecordChange, true);
    document[m]('keydown', onRecordKey, true);
  }

  globalThis.__bmcp = {
    snapshot, resolve, describe, prepare, beginFill, focusForTyping, selectOptions, checkedState,
    waitForText, generateLocator, isRoleVisible, highlight, removeHighlight, showHandoff, hideHandoff,
    setRecording, roleOf, nameOf, isVisible, pageText,
  };
})();
