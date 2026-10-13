/* Local documentation search, section navigation and screenshot zoom. No remote dependencies. */
(() => {
  'use strict';

  const normalize = (value) => String(value).normalize('NFD').replace(/[\u0300-\u036f]/g, '').toLowerCase();
  const words = (value) => normalize(String(value).replace(/([a-z])([A-Z])/g, '$1 $2')).match(/[\p{L}\p{N}]+/gu) || [];
  const queryWords = (value) => [...new Set(words(value.slice(0, 200)))];
  const prepareDocuments = (documents) => documents.map((document, order) => ({
    ...document, order,
    fields: ['title', 'tags', 'summary', 'body'].map((key) => {
      const text = Array.isArray(document[key]) ? document[key].join(' ') : document[key];
      return { text: normalize(text), words: words(text) };
    })
  }));

  const searchDocuments = (documents, query) => {
    const terms = queryWords(query);
    if (!terms.length) return query.trim() ? [] : documents.map((document) => ({ document, score: 0 }));
    const weights = [42, 32, 15, 5];
    const phrase = normalize(query.trim().replace(/^--/, ''));
    return documents.map((document) => {
      let score = 0;
      for (const term of terms) {
        let best = 0;
        const singular = term.length > 4 && term.endsWith('s') && !term.endsWith('ss') ? term.slice(0, -1) : null;
        document.fields.forEach((field, index) => {
          const exact = field.words.includes(term);
          const plural = singular && field.words.includes(singular);
          const prefix = term.length > 1 && field.words.some((word) => word.startsWith(term));
          const contains = term.length > 2 && field.text.includes(term);
          if (exact || plural || prefix || contains) best = Math.max(best, weights[index] * (exact ? 1 : plural ? 0.9 : prefix ? 0.8 : 0.5));
        });
        if (!best) return null;
        score += best;
      }
      if (document.fields[0].text === phrase) score += 100;
      else if (document.fields[0].text.includes(phrase)) score += 45;
      if (document.fields[2].text.includes(phrase)) score += 8;
      return { document, score };
    }).filter(Boolean).sort((a, b) => b.score - a.score || a.document.order - b.document.order);
  };

  const snippet = (document, query, length = 185) => {
    const terms = queryWords(query);
    const summary = document.summary;
    if (!terms.length || terms.some((term) => normalize(summary).includes(term))) {
      return summary.length > length ? `${summary.slice(0, length).replace(/\s+\S*$/, '')}…` : summary;
    }
    const body = document.body;
    const positions = terms.map((term) => normalize(body).indexOf(term)).filter((position) => position >= 0);
    const position = positions.length ? Math.min(...positions) : 0;
    let start = Math.max(0, position - 55);
    if (start) {
      const boundary = body.indexOf(' ', start);
      if (boundary >= 0 && boundary < position) start = boundary + 1;
    }
    const excerpt = start + length < body.length ? body.slice(start, start + length).replace(/\s+\S*$/, '') : body.slice(start);
    return `${start ? '…' : ''}${excerpt}${start + length < body.length ? '…' : ''}`;
  };

  // Keep ranking testable without a browser or a JavaScript package install.
  if (typeof module !== 'undefined' && module.exports) module.exports = { normalize, words, prepareDocuments, searchDocuments, snippet };
  if (typeof document === 'undefined') return;

  const element = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const sections = [...document.querySelectorAll('[data-doc-section]')];
  let indexPromise;
  const readIndex = () => {
    if (!indexPromise) {
      indexPromise = fetch('search-index.json')
        .then((response) => {
          if (!response.ok) throw new Error('search index unavailable');
          return response.json();
        })
        .then((data) => {
          if (data.schema_version !== 1 || !Array.isArray(data.documents) || !data.documents.length) throw new Error('invalid search index');
          if (!data.documents.every((item) => item && ['id', 'title', 'summary', 'category', 'body', 'url'].every((key) => typeof item[key] === 'string') && Array.isArray(item.tags) && item.tags.every((tag) => typeof tag === 'string') && /^docs\.html#[a-z][a-z0-9-]*$/.test(item.url))) throw new Error('invalid search document');
          return { documents: prepareDocuments(data.documents), local: false };
        })
        .catch(() => {
          // File previews and interrupted requests can still search the rendered documentation.
          const documents = sections.map((section) => ({
            id: section.id, title: section.dataset.docTitle,
            category: section.dataset.docCategory || 'Documentation',
            tags: (section.dataset.docTags || '').split(',').map((tag) => tag.trim()),
            summary: section.querySelector('.doc-summary')?.textContent.trim() || '',
            body: section.textContent.replace(/\s+/g, ' ').trim(), url: `docs.html#${section.id}`
          }));
          return { documents: prepareDocuments(documents), local: true };
        });
    }
    return indexPromise;
  };

  const makeResults = (input, container, status, options = {}) => {
    let selected = -1;
    let links = [];
    let request = 0;
    container.setAttribute('role', 'listbox');
    container.setAttribute('aria-label', 'Documentation search results');
    input.setAttribute('role', 'combobox');
    input.setAttribute('aria-autocomplete', 'list');
    input.setAttribute('aria-expanded', 'false');
    input.setAttribute('aria-controls', container.id);

    const select = (index) => {
      selected = index;
      links.forEach((link, position) => {
        link.setAttribute('aria-selected', String(position === index));
        if (position === index) link.setAttribute('aria-current', 'true');
        else link.removeAttribute('aria-current');
      });
      if (links[index]) {
        input.setAttribute('aria-activedescendant', links[index].id);
        links[index].scrollIntoView({ block: 'nearest' });
      } else input.removeAttribute('aria-activedescendant');
    };

    const render = async () => {
      const current = ++request;
      const query = input.value.trim();
      if (options.onQuery) options.onQuery(query);
      if (!query && !options.suggestions) {
        container.replaceChildren();
        container.hidden = true;
        input.setAttribute('aria-expanded', 'false');
        input.removeAttribute('aria-activedescendant');
        links = []; selected = -1;
        status.textContent = 'Search guides, controls and commands.';
        return;
      }
      status.textContent = 'Searching documentation…';
      const data = await readIndex();
      if (current !== request) return;
      const matches = searchDocuments(data.documents, query);
      const visible = matches.slice(0, options.limit || 30);
      container.replaceChildren();
      container.hidden = false;
      links = []; selected = -1;
      input.removeAttribute('aria-activedescendant');
      if (!data.documents.length) {
        status.textContent = 'Search is unavailable here. Open the documentation and use your browser’s Find (Ctrl/Cmd F).';
        const fallback = element('a', 'search-result', 'Open the documentation');
        fallback.href = 'docs.html';
        container.append(fallback);
        input.setAttribute('aria-expanded', 'false');
        return;
      }
      visible.forEach(({ document: result }, index) => {
        const link = element('a', 'search-result');
        link.href = result.url;
        link.id = `${container.id}-${index}`;
        link.setAttribute('role', 'option');
        link.setAttribute('aria-selected', 'false');
        link.append(element('span', 'search-result-meta', result.category),
          element('span', 'search-result-title', result.title),
          element('span', 'search-result-summary', snippet(result, query)));
        if (options.onNavigate) link.addEventListener('click', options.onNavigate);
        container.append(link); links.push(link);
      });
      input.setAttribute('aria-expanded', String(links.length > 0));
      status.textContent = !query ? 'Start typing to search, or open a guide below.' : matches.length
        ? `${matches.length} result${matches.length === 1 ? '' : 's'}${matches.length > visible.length ? ` · Showing the first ${visible.length}` : ''}.`
        : 'No results. Try a feature name, command or shorter search, such as “exposure”, “S3” or “session”.';
      if (data.local) status.textContent += ' Searching this page.';
    };

    input.addEventListener('input', render);
    input.addEventListener('keydown', (event) => {
      if (event.key === 'ArrowDown' && links.length) {
        event.preventDefault(); select((selected + 1) % links.length);
      } else if (event.key === 'ArrowUp' && links.length) {
        event.preventDefault(); select(selected <= 0 ? links.length - 1 : selected - 1);
      } else if (event.key === 'Enter' && links.length) {
        event.preventDefault(); links[selected < 0 ? 0 : selected].click();
      }
    });
    return render;
  };

  const inlineInput = document.getElementById('docs-query');
  let inlineRender;
  if (inlineInput) {
    const results = document.getElementById('docs-results');
    const status = document.getElementById('docs-search-status');
    if (results && status) {
      inlineRender = makeResults(inlineInput, results, status);
      const query = new URLSearchParams(location.search).get('q');
      if (query) inlineInput.value = query.slice(0, 200);
      inlineRender();
      inlineInput.closest('form')?.addEventListener('submit', (event) => event.preventDefault());
      if (query || location.hash === '#search') inlineInput.focus({ preventScroll: false });
    }
  }

  let searchDialog;
  let searchInput;
  let searchRender;
  let searchOpener;
  const openSearch = (opener) => {
    if (typeof HTMLDialogElement === 'undefined') {
      location.href = 'docs.html#search';
      return;
    }
    if (!searchDialog) {
      searchDialog = element('dialog', 'search-dialog');
      searchDialog.id = 'site-search';
      searchDialog.setAttribute('aria-labelledby', 'search-dialog-title');
      const heading = element('div', 'search-dialog-heading');
      const title = element('h2', '', 'Search documentation');
      title.id = 'search-dialog-title';
      const close = element('button', 'search-close', 'Close');
      close.type = 'button'; close.setAttribute('aria-label', 'Close documentation search');
      close.addEventListener('click', () => searchDialog.close());
      heading.append(title, close);
      const label = element('label', 'search-input-wrap');
      label.append(element('span', 'search-label', 'Feature, task or command'));
      searchInput = element('input', 'search-input');
      searchInput.id = 'site-search-query'; searchInput.type = 'search';
      searchInput.placeholder = 'Search exposure, S3, --destination…';
      searchInput.autocomplete = 'off'; searchInput.spellcheck = false;
      label.append(searchInput);
      const status = element('p', 'search-status');
      status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
      const results = element('div', 'search-results'); results.id = 'site-search-results';
      results.setAttribute('aria-label', 'Documentation search results');
      const footer = element('div', 'search-dialog-footer');
      const allResults = element('a', '', 'Browse all documentation'); allResults.href = 'docs.html';
      footer.append(element('span', '', '↑ ↓ to choose · Enter to open · Esc to close'), allResults);
      searchDialog.append(heading, label, status, results, footer);
      document.body.append(searchDialog);
      searchRender = makeResults(searchInput, results, status, {
        suggestions: true, limit: 10,
        onNavigate: () => searchDialog.close(),
        onQuery: (query) => {
          allResults.href = query ? `docs.html?q=${encodeURIComponent(query)}#search` : 'docs.html';
          allResults.textContent = query ? 'View all results' : 'Browse all documentation';
        }
      });
      searchDialog.addEventListener('close', () => searchOpener?.focus({ preventScroll: true }));
      searchDialog.addEventListener('click', (event) => {
        if (event.target !== searchDialog) return;
        const bounds = searchDialog.getBoundingClientRect();
        if (event.clientX < bounds.left || event.clientX > bounds.right || event.clientY < bounds.top || event.clientY > bounds.bottom) searchDialog.close();
      });
    }
    searchOpener = opener || document.activeElement;
    if (inlineInput) searchInput.value = inlineInput.value;
    searchDialog.showModal();
    searchInput.focus(); searchInput.select(); searchRender();
  };

  document.querySelectorAll('a.search-trigger').forEach((trigger) => {
    trigger.setAttribute('aria-haspopup', 'dialog');
    trigger.addEventListener('click', (event) => {
      if (event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      event.preventDefault(); openSearch(trigger);
    });
  });
  document.addEventListener('keydown', (event) => {
    const target = event.target;
    const editing = target instanceof Element && (target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])'));
    if (!editing && ((event.key.toLowerCase() === 'k' && (event.metaKey || event.ctrlKey) && !event.altKey) ||
      (event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey))) {
      if (document.querySelector('dialog[open]')) return;
      event.preventDefault(); openSearch(document.activeElement);
    }
  });

  const sidebar = [...document.querySelectorAll('.docs-sidebar a[href]')];
  const activateSection = (id) => sidebar.forEach((link) => {
    if (link.hash === `#${id}`) link.setAttribute('aria-current', 'location');
    else link.removeAttribute('aria-current');
  });
  if (sections.length && sidebar.length && 'IntersectionObserver' in window) {
    const visible = new Set();
    const observer = new IntersectionObserver((entries) => {
      entries.forEach((entry) => entry.isIntersecting ? visible.add(entry.target) : visible.delete(entry.target));
      const active = [...visible].sort((a, b) => Math.abs(a.getBoundingClientRect().top - 110) - Math.abs(b.getBoundingClientRect().top - 110))[0];
      if (active) activateSection(active.id);
    }, { rootMargin: '-95px 0px -55% 0px', threshold: 0 });
    sections.forEach((section) => observer.observe(section));
    sidebar.forEach((link) => link.addEventListener('click', () => activateSection(link.hash.slice(1))));
  }
  document.querySelectorAll('.site-header nav a[href]').forEach((link) => {
    const page = location.pathname.split('/').pop() || 'index.html';
    if ((link.pathname.split('/').pop() || 'index.html') === page && !link.hash && !link.classList.contains('search-trigger')) link.setAttribute('aria-current', 'page');
  });

  let screenshotDialog;
  let screenshotImage;
  let screenshotCaption;
  let screenshotOpener;
  document.querySelectorAll('a[data-screenshot], a.screenshot-link').forEach((link) => link.addEventListener('click', (event) => {
    if (typeof HTMLDialogElement === 'undefined' || event.button || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    const source = link.querySelector('img');
    if (!source) return;
    event.preventDefault();
    if (!screenshotDialog) {
      screenshotDialog = element('dialog', 'screenshot-dialog');
      screenshotDialog.setAttribute('aria-label', 'App screenshot');
      const close = element('button', 'search-close', 'Close');
      close.type = 'button'; close.setAttribute('aria-label', 'Close screenshot');
      close.addEventListener('click', () => screenshotDialog.close());
      screenshotImage = element('img', 'screenshot-dialog-image');
      screenshotCaption = element('p', 'screenshot-dialog-caption'); screenshotCaption.id = 'screenshot-dialog-caption';
      screenshotDialog.setAttribute('aria-describedby', screenshotCaption.id);
      screenshotDialog.append(close, screenshotImage, screenshotCaption);
      screenshotDialog.addEventListener('close', () => screenshotOpener?.focus({ preventScroll: true }));
      document.body.append(screenshotDialog);
    }
    screenshotImage.src = link.href;
    screenshotImage.alt = source.alt;
    screenshotCaption.textContent = link.closest('figure')?.querySelector('figcaption')?.textContent.trim() || source.alt;
    screenshotOpener = link;
    screenshotDialog.showModal();
  }));
})();
