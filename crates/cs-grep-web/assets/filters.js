// The venue filter UI: a Filters button opening a dialog, and removable pills for what is on.
// It owns the filter state and the `venues`/`rank` URL parameters, and tells the page through
// `onChange` whenever the searched venues change.

import { byId, el } from './dom.js';
import { VenueSelection } from './venue-selection.js';

const dom = {
  button: byId('filter-button'),
  count: byId('filter-count'),
  dialog: byId('filter-dialog'),
  scope: byId('filter-scope'),
  reset: byId('reset-filters'),
  pills: byId('pills'),
  topics: byId('topics'),
  rank: byId('rank-select'),
  venueSearch: byId('venue-search'),
  venueGroups: byId('venue-groups'),
  venuesEmpty: byId('venues-empty'),
  selectAll: byId('select-all'),
  selectNone: byId('select-none'),
};
const urlParams = new URLSearchParams(location.search);
let selection = null; // Set once the venue catalog has loaded.
let notifyChange = () => {};

// Venue ids for the search API, or null for all venues. Until the catalog loads, the URL's
// choice is passed through as-is; shortcut: the rank part is ignored without the catalog.
export function venueParam() {
  return selection ? selection.searchParam() : urlParams.get('venues');
}

export function scopeText() {
  return selection?.scope() ?? '';
}

// Loads the catalog and shows the Filters button. Search keeps working if this fails.
export async function initFilters(onChange) {
  notifyChange = onChange;
  try {
    const { venues, tags } = await (await fetch('api/venues')).json();
    selection = VenueSelection.fromUrl(venues, tags, urlParams.get('venues'), urlParams.get('rank'));
  } catch {
    return;
  }
  buildDialog();
  bindDialog();
  render();
  dom.button.hidden = false;
}

function update(change) {
  change();
  syncUrl();
  render();
  notifyChange();
}

function syncUrl() {
  const { venues, rank } = selection.urlParams();
  const params = new URLSearchParams(location.search);
  params.delete('venues');
  params.delete('rank');
  if (venues != null) params.set('venues', venues);
  if (rank) params.set('rank', rank);
  // Shared links keep readable commas instead of %2C.
  const query = params.toString().replaceAll('%2C', ',');
  history.replaceState(null, '', `${location.pathname}${query ? `?${query}` : ''}${location.hash}`);
}

// Building the dialog, once per catalog

function buildDialog() {
  dom.topics.append(...selection.topics().map((tag) => {
    const chip = checkbox('chip', selection.topicLabel(tag), () => update(() => selection.toggleTopic(tag)));
    chip.topic = tag;
    return chip;
  }));
  // Opening the dialog focuses the first topic, not the close button; on phones this also
  // avoids popping up the keyboard, as focusing the venue search would.
  dom.topics.querySelector('input')?.setAttribute('autofocus', '');
  dom.rank.append(...selection.rankOptions().map(({ rank, label }) => new Option(label, rank ?? '')));
  dom.rank.closest('fieldset').hidden = !dom.rank.options.length;
  dom.venueGroups.replaceChildren(...selection.groups().map(({ label, venues }) => {
    const group = el('fieldset');
    group.append(el('legend', '', label), ...venues.map(venueOption));
    return group;
  }));
}

function venueOption(venue) {
  const option = checkbox('option', el('b', '', venue.id), (checked) =>
    update(() => selection.toggleVenues([venue], checked)));
  option.venue = venue;
  const name = el('span', '', venue.name);
  name.title = venue.name; // The name is often cut off in the narrow columns.
  option.append(name, el('small'));
  return option;
}

function checkbox(className, content, onChange) {
  const label = el('label', className);
  const input = el('input');
  input.type = 'checkbox';
  input.addEventListener('change', () => onChange(input.checked));
  label.append(input, content);
  return label;
}

function bindDialog() {
  dom.button.addEventListener('click', () => dom.dialog.showModal());
  dom.reset.addEventListener('click', () => update(() => selection.reset()));
  dom.rank.addEventListener('change', () => update(() => selection.chooseRank(dom.rank.value || null)));
  dom.venueSearch.addEventListener('input', filterVenueList);
  // Both buttons act on the visible venues, which are all venues when not searching.
  dom.selectAll.addEventListener('click', () => update(() => selection.choose(visibleVenues())));
  dom.selectNone.addEventListener('click', () => update(() => selection.toggleVenues(visibleVenues(), false)));
  // Reset the venue search on close, so the next opening shows the whole list.
  dom.dialog.addEventListener('close', () => {
    dom.venueSearch.value = '';
    filterVenueList();
  });
  // A phone keyboard shrinks only the visual viewport, and panning moves it, so lift the
  // bottom sheet above the keyboard on both.
  visualViewport.addEventListener('resize', liftAboveKeyboard);
  visualViewport.addEventListener('scroll', liftAboveKeyboard);
}

function liftAboveKeyboard() {
  const keyboard = Math.max(0, window.innerHeight - visualViewport.height - visualViewport.offsetTop);
  dom.dialog.style.setProperty('--keyboard', `${keyboard}px`);
}

// Rendering, in place so keyboard focus inside the dialog survives

function render() {
  dom.topics.querySelectorAll('.chip').forEach((chip) => {
    chip.querySelector('input').checked = selection.activeTopics.has(chip.topic);
  });
  dom.rank.value = selection.minRank ?? '';
  // Venues outside the rank filter are dimmed; chosen ones also say why they are not searched.
  dom.venueGroups.querySelectorAll('.option').forEach((option) => {
    const { venue } = option;
    const chosen = selection.venues.has(venue.id);
    const outranked = !selection.rankAllowed(venue);
    option.classList.toggle('outranked', outranked);
    option.querySelector('small').textContent = chosen && outranked ? 'excluded by rank' : venue.rank ?? '';
    option.querySelector('input').checked = chosen;
  });
  renderSummary();
}

function renderSummary() {
  const filters = selection.activeFilters();
  dom.scope.textContent = `· ${selection.searched().length} of ${selection.catalog.length} venues`;
  dom.count.textContent = filters.length || '';
  dom.button.classList.toggle('active', filters.length > 0);
  dom.button.setAttribute('aria-label', filters.length ? `Filters, ${filters.length} active` : 'Filters');
  const pills = filters.map(({ label, remove }) => pill(`${label} ×`, `Remove filter ${label}`, remove));
  if (pills.length > 1) pills.push(pill('Clear all', 'Clear all filters', () => selection.reset()));
  dom.pills.replaceChildren(...pills);
}

function pill(text, ariaLabel, change) {
  const button = el('button', 'pill', text);
  button.setAttribute('aria-label', ariaLabel);
  // The used pill disappears, so focus moves to the pill now in its place, else the Filters button.
  button.addEventListener('click', () => {
    const index = [...dom.pills.children].indexOf(button);
    update(change);
    (dom.pills.children[index] ?? dom.pills.children[index - 1] ?? dom.button).focus();
  });
  return button;
}

// Venue search, a view filter on the list that never changes the selection by itself

// Compares letters and digits only, so "euro s&p" finds EuroSP.
const searchable = (text) => text.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');

const visibleVenues = () => [...dom.venueGroups.querySelectorAll('.option:not([hidden])')]
  .map((option) => option.venue);

// While searching, the list is flat, without group headings.
function filterVenueList() {
  const query = searchable(dom.venueSearch.value);
  dom.venueGroups.querySelectorAll('.option').forEach((option) => {
    const { venue } = option;
    option.hidden = ![venue.id, venue.name, ...venue.aliases].some((text) => searchable(text).includes(query));
  });
  dom.venueGroups.querySelectorAll('fieldset').forEach((group) => {
    group.hidden = !group.querySelector('.option:not([hidden])');
    group.querySelector('legend').hidden = query !== '';
  });
  const shown = visibleVenues().length;
  dom.venuesEmpty.hidden = shown > 0;
  dom.selectAll.textContent = query ? 'Only these' : 'Select all';
  dom.selectNone.textContent = query ? 'Remove these' : 'Clear';
  dom.selectAll.disabled = dom.selectNone.disabled = shown === 0;
}
