import { byId, el, setUrlParams } from './dom.js';
import { initFilters, scopeText, venueParam } from './filters.js';

const PAGE_SIZE = 120;
const LOAD_MARGIN = 200;
const SEARCH_DEBOUNCE_MS = 300;
const dom = {
  search: byId('search'),
  sortButtons: document.querySelectorAll('[data-sort]'),
  status: byId('status'),
  results: byId('results'),
  sentinel: byId('sentinel'),
  detail: byId('detail'),
  modal: byId('bibtex-modal'),
  modalCode: byId('bibtex-code'),
  modalStatus: byId('modal-status'),
  modalClose: byId('modal-close'),
  modalCopy: byId('modal-copy'),
  modalDownload: byId('modal-download'),
  themeToggle: byId('theme-toggle'),
};
const state = {
  sort: 'year',
  results: [],
  ranks: {},
  selected: null,
  nextOffset: null,
  total: null, // All papers matching the current search, as counted by the server.
  loading: false,
};
let searchController;
let bibtexController;
let debounceTimer;
let bibtex = '';
let bibtexKey = '';

function startSearch(delay = 0) {
  clearTimeout(debounceTimer);
  searchController?.abort();
  // The old selection and next page belong to the previous query.
  markRow(dom.results.querySelector('.selected'), false);
  state.selected = null;
  state.nextOffset = null;
  state.loading = true;
  renderDetail();
  renderStatus();
  debounceTimer = setTimeout(() => {
    // The search text and sort live in the URL too, so a shared link reproduces the view.
    setUrlParams({ q: dom.search.value || null, sort: state.sort === 'year' ? null : state.sort });
    loadPage(true);
  }, delay);
}

function loadIfNearEnd() {
  const viewport = dom.results.getBoundingClientRect();
  if (dom.sentinel.getBoundingClientRect().top <= viewport.bottom + LOAD_MARGIN) loadPage(false);
}

async function loadPage(reset) {
  if (!reset && (state.loading || state.nextOffset == null)) return;
  searchController?.abort();
  const controller = new AbortController();
  searchController = controller;
  state.loading = true;
  renderStatus();
  const params = new URLSearchParams({
    q: dom.search.value,
    sort: state.sort,
    limit: PAGE_SIZE,
    offset: reset ? 0 : state.nextOffset,
  });
  const venues = venueParam();
  if (venues != null) params.set('venues', venues);
  let error = '';
  let status = 0;
  try {
    const response = await fetch(`api/search?${params}`, { signal: controller.signal });
    status = response.status;
    const data = await response.json().catch(() => ({}));
    if (!response.ok || !Array.isArray(data.papers)) {
      throw new Error(data.error || `Search failed (${status})`);
    }
    if (controller.signal.aborted) return;
    state.ranks = data.ranks || {};
    if (reset) {
      state.results = [];
      state.selected = data.papers[0] || null;
      dom.results.replaceChildren(dom.sentinel);
      dom.results.removeAttribute('aria-activedescendant');
      dom.results.scrollTop = 0;
    }
    dom.sentinel.before(createRows(data.papers, state.results.length));
    state.results.push(...data.papers);
    state.nextOffset = data.next_offset;
    if (reset) state.total = data.total;
    if (reset) renderDetail();
  } catch (e) {
    error = e.message || 'Search failed';
  } finally {
    if (!controller.signal.aborted) {
      state.loading = false;
      // Retrying a rejected query can't help; network and server errors may clear up.
      const retryable = error && (status < 400 || status >= 500);
      renderStatus(error, retryable ? () => loadPage(reset) : null);
      if (!error) loadIfNearEnd();
    }
  }
}

function createRows(papers, offset) {
  const fragment = document.createDocumentFragment();
  papers.forEach((paper, index) => {
    const row = el('div', 'result-row');
    row.id = `result-${offset + index}`;
    row.dataset.index = offset + index;
    row.setAttribute('role', 'option');
    row.setAttribute('aria-selected', 'false');
    row.appendChild(el('span', 'col-venue', paper.venue || ''));
    row.appendChild(el('span', 'col-year', paper.year ?? ''));
    const rank = state.ranks[paper.venue];
    row.appendChild(el('span', 'col-rank', rank ? `[${rank}]` : ''));
    row.appendChild(el('span', 'col-title', paper.title || ''));
    row.appendChild(el('span', 'col-authors', paper.authors || ''));
    fragment.appendChild(row);
    if (paper === state.selected) markRow(row, true);
  });
  return fragment;
}

function markRow(row, selected) {
  if (!row) return;
  row.classList.toggle('selected', selected);
  row.setAttribute('aria-selected', selected);
  if (selected) dom.results.setAttribute('aria-activedescendant', row.id);
  else dom.results.removeAttribute('aria-activedescendant');
}

function setSelected(index) {
  const next = Math.max(0, Math.min(index, state.results.length - 1));
  if (state.results[next] === state.selected) return;
  markRow(dom.results.querySelector('.selected'), false);
  state.selected = state.results[next];
  const row = byId(`result-${next}`);
  markRow(row, true);
  row.scrollIntoView({ block: 'nearest' });
  // Re-rendering the detail pane would drop focus that is inside it.
  if (dom.detail.contains(document.activeElement)) dom.results.focus();
  renderDetail();
}

function paperUrl(paper) {
  for (const value of [paper.url, paper.doi ? `https://doi.org/${paper.doi}` : '']) {
    try {
      const url = new URL(value);
      if (url.protocol === 'http:' || url.protocol === 'https:') return url.href;
    } catch { /* Ignore missing or malformed publisher links. */ }
  }
  return '';
}

function renderDetail() {
  dom.detail.replaceChildren();
  dom.detail.scrollTop = 0;
  const paper = state.selected;
  if (!paper) {
    dom.detail.appendChild(el('div', 'detail-empty', 'No paper selected'));
    return;
  }
  const content = el('div', 'detail-content');
  content.appendChild(el('h2', 'detail-title', paper.title || ''));
  const meta = [paper.venue, paper.year];
  const rank = state.ranks[paper.venue];
  if (rank) meta.push(`rank ${rank}`);
  content.appendChild(el('div', 'detail-meta', meta.filter(Boolean).join(' · ')));
  content.appendChild(labeledBlock('detail-authors', 'Authors', paper.authors || ''));
  if (paper.doi) content.appendChild(labeledBlock('detail-link', 'DOI', paper.doi));
  const url = paperUrl(paper);
  if (url) {
    const block = labeledBlock('detail-link', 'URL', '');
    const link = el('a', '', url);
    link.href = url;
    link.target = '_blank';
    link.rel = 'noreferrer';
    block.appendChild(link);
    content.appendChild(block);
  }
  const citation = labeledBlock('detail-link', 'Citation', '');
  const button = el('button', 'download-btn', 'BibTeX');
  button.addEventListener('click', () => openBibtex(paper));
  citation.appendChild(button);
  content.appendChild(citation);
  const abstract = el('div', 'detail-abstract');
  abstract.appendChild(el('strong', '', 'Abstract'));
  abstract.appendChild(el('p', '', paper.abstract || 'No abstract available.'));
  content.appendChild(abstract);
  dom.detail.appendChild(content);
}

// Errors are passed in rather than stored, so the next render clears them and their Retry button.
function renderStatus(error = '', retry = null) {
  dom.results.setAttribute('aria-busy', state.loading);
  if (error) {
    dom.status.replaceChildren(el('span', 'error', error));
    if (retry) {
      const button = el('button', 'modal-btn', 'Retry');
      button.addEventListener('click', () => {
        dom.search.focus();
        retry();
      });
      dom.status.appendChild(button);
    }
    return;
  }
  // A new search has no next page yet, and the old count no longer applies.
  if (state.loading && state.nextOffset == null) {
    dom.status.replaceChildren(el('span', '', 'Loading…'));
    return;
  }
  const count = state.total;
  const summary = count ? `${count.toLocaleString('en')} paper${count === 1 ? '' : 's'}` : 'No papers found';
  dom.status.replaceChildren(el('span', '', summary));
  const scope = scopeText();
  if (scope) dom.status.appendChild(el('span', '', scope));
  if (state.loading) dom.status.appendChild(el('span', 'spinner', 'Loading…'));
}

function onNavigation(event) {
  // An open dialog has its own controls, which result shortcuts would hijack.
  if (event.defaultPrevented || event.isComposing || document.querySelector('dialog[open]')) return;
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  if (event.target === dom.results && (event.key.length === 1 || event.key === 'Backspace')) {
    // Typing in the result list goes to the search box.
    dom.search.focus();
  } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    if (!state.results.length) return;
    event.preventDefault();
    const next = state.results.indexOf(state.selected) + (event.key === 'ArrowDown' ? 1 : -1);
    if (next >= state.results.length - 5) loadPage(false);
    setSelected(next);
  } else if (event.key === 'Enter' && !event.target.closest('button, a')) {
    event.preventDefault();
    openSelected();
  }
}

function openSelected() {
  const paper = state.selected;
  if (!paper) return;
  const url = paperUrl(paper);
  if (url) window.open(url, '_blank', 'noopener,noreferrer');
}

async function openBibtex(paper) {
  bibtexController?.abort();
  const controller = new AbortController();
  bibtexController = controller;
  bibtex = '';
  bibtexKey = paper.dblp_key;
  dom.modalCode.textContent = 'Loading…';
  dom.modalStatus.textContent = '';
  dom.modalCopy.disabled = dom.modalDownload.disabled = true;
  if (!dom.modal.open) dom.modal.showModal();
  try {
    const url = `api/bibtex?key=${encodeURIComponent(paper.dblp_key)}`;
    const response = await fetch(url, { signal: controller.signal });
    if (!response.ok) {
      const data = await response.json().catch(() => ({}));
      throw new Error(data.error || `BibTeX failed (${response.status})`);
    }
    const text = await response.text();
    if (controller.signal.aborted) return;
    bibtex = text;
    dom.modalCode.textContent = text;
    dom.modalCopy.disabled = dom.modalDownload.disabled = false;
  } catch (error) {
    if (controller.signal.aborted) return;
    dom.modalCode.textContent = '';
    dom.modalStatus.textContent = error.message || 'Failed to fetch BibTeX';
  }
}

async function copyBibtex() {
  const controller = bibtexController;
  try {
    await navigator.clipboard.writeText(bibtex);
    if (controller.signal.aborted) return;
    dom.modalStatus.textContent = 'Copied!';
  } catch {
    if (controller.signal.aborted) return;
    dom.modalStatus.textContent = 'Copy failed. Select the citation above or download it.';
  }
}

function downloadBibtex() {
  const url = URL.createObjectURL(new Blob([bibtex], { type: 'text/plain' }));
  const link = el('a');
  link.href = url;
  link.download = `${bibtexKey.split('/').pop()}.bib`;
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 0);
  dom.modalStatus.textContent = '';
}

function labeledBlock(className, label, text) {
  const block = el('div', className);
  block.appendChild(el('strong', '', label));
  block.appendChild(document.createTextNode(` ${text}`));
  return block;
}

dom.search.addEventListener('input', () => startSearch(SEARCH_DEBOUNCE_MS));
document.addEventListener('keydown', onNavigation);
dom.results.addEventListener('click', (event) => {
  const row = event.target.closest('.result-row');
  if (row) setSelected(Number(row.dataset.index));
});
dom.results.addEventListener('dblclick', (event) => {
  if (event.target.closest('.result-row')) openSelected();
});
new IntersectionObserver(loadIfNearEnd, { root: dom.results, rootMargin: `${LOAD_MARGIN}px` })
  .observe(dom.sentinel);
function setSort(sort) {
  state.sort = sort;
  dom.sortButtons.forEach((item) => {
    item.classList.toggle('active', item.dataset.sort === sort);
    item.setAttribute('aria-pressed', item.dataset.sort === sort);
  });
}

dom.sortButtons.forEach((button) => {
  button.addEventListener('click', () => {
    if (state.sort === button.dataset.sort) return;
    setSort(button.dataset.sort);
    startSearch();
  });
});
dom.modalClose.addEventListener('click', () => dom.modal.close());
// Clicking the backdrop closes a dialog in every browser. Only a press that starts on the
// backdrop counts, so dragging a text selection out of a dialog doesn't close it.
document.querySelectorAll('dialog').forEach((dialog) => {
  let pressedBackdrop = false;
  dialog.addEventListener('mousedown', (event) => { pressedBackdrop = event.target === dialog; });
  dialog.addEventListener('click', (event) => {
    if (pressedBackdrop && event.target === dialog) dialog.close();
  });
});
dom.modal.addEventListener('close', () => {
  // Reopening before this event fires has already started a new request.
  if (!dom.modal.open) bibtexController?.abort();
});
dom.modalCopy.addEventListener('click', copyBibtex);
dom.modalDownload.addEventListener('click', downloadBibtex);
dom.themeToggle.addEventListener('click', () => {
  const theme = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
  document.documentElement.dataset.theme = theme;
  localStorage.setItem('theme', theme);
});
window.matchMedia('(prefers-color-scheme: light)').addEventListener('change', (event) => {
  if (localStorage.getItem('theme')) return;
  document.documentElement.dataset.theme = event.matches ? 'light' : 'dark';
});
// Restore a shared view: search text and sort from the URL, filters in initFilters.
const urlParams = new URLSearchParams(location.search);
dom.search.value = urlParams.get('q') ?? '';
const sortFromUrl = urlParams.get('sort');
if ([...dom.sortButtons].some((button) => button.dataset.sort === sortFromUrl)) setSort(sortFromUrl);
initFilters(startSearch).then(() => startSearch());
dom.search.focus();
