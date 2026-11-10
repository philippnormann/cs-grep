// The venue filter model, kept free of DOM code so it is easy to read and test.
// Topics and single venues choose a venue set, and a minimum rank narrows it. Topics combine
// into a union of their venues; editing single venues makes a custom selection.

const RANKS = ['A*', 'A', 'B', 'C']; // Best first, as in the CORE ranking.
const OTHER = ''; // Venue list group for venues without tags.

export class VenueSelection {
  constructor(catalog, tagLabels) {
    this.catalog = catalog; // Venues from the server config, in config order.
    this.tagLabels = tagLabels;
    this.venues = new Set(catalog.map((venue) => venue.id)); // Chosen venue ids.
    this.activeTopics = new Set(); // Topics that build the selection; empty for a custom one.
    this.minRank = null; // null means any rank.
  }

  // Restores a selection from `?venues=…&rank=…`, accepting venue aliases too. Ids that are no
  // longer in the catalog are dropped, so old links keep working after a venue is removed.
  static fromUrl(catalog, tagLabels, venuesParam, rankParam) {
    const selection = new VenueSelection(catalog, tagLabels);
    if (venuesParam != null) {
      const wanted = venuesParam.toLowerCase().split(',');
      const named = (venue) => [venue.id, ...venue.aliases].some((name) => wanted.includes(name.toLowerCase()));
      const found = catalog.filter(named);
      // An empty `?venues=` means none; a link whose venues are all gone means all again.
      if (found.length || venuesParam === '') selection.choose(found);
    }
    // Only ranks this catalog offers, so the rank select can always show the restored choice.
    if (rankParam && selection.rankOptions().some((option) => option.rank === rankParam)) selection.minRank = rankParam;
    return selection;
  }

  topicLabel(tag) {
    return Object.hasOwn(this.tagLabels, tag) ? this.tagLabels[tag] : tag;
  }

  topics() {
    return [...new Set(this.catalog.flatMap((venue) => venue.tags))]
      .sort((a, b) => this.topicLabel(a).localeCompare(this.topicLabel(b)));
  }

  venuesWithTag(tag) {
    return this.catalog.filter((venue) => venue.tags.includes(tag));
  }

  // Venue list groups: each venue once, under its first tag, untagged venues last.
  groups() {
    return [...this.topics(), OTHER]
      .map((tag) => ({
        label: tag === OTHER ? 'Other' : this.topicLabel(tag),
        venues: this.catalog.filter((venue) => (venue.tags[0] ?? OTHER) === tag),
      }))
      .filter((group) => group.venues.length);
  }

  // The rank choices this catalog supports, e.g. Any rank, A* only, A and up.
  rankOptions() {
    const ranks = RANKS.filter((rank) => this.catalog.some((venue) => venue.rank === rank));
    if (!ranks.length) return [];
    return [null, ...ranks].map((rank, index) => ({
      rank,
      label: rank === null ? 'Any rank' : index === 1 ? `${rank} only` : `${rank} and up`,
    }));
  }

  allChosen() {
    return this.venues.size === this.catalog.length;
  }

  rankAllowed(venue) {
    if (!this.minRank) return true;
    return RANKS.includes(venue.rank) && RANKS.indexOf(venue.rank) <= RANKS.indexOf(this.minRank);
  }

  searched() {
    return this.catalog.filter((venue) => this.venues.has(venue.id) && this.rankAllowed(venue));
  }

  searchParam() {
    const searched = this.searched();
    return searched.length === this.catalog.length ? null : searched.map((venue) => venue.id).join(',');
  }

  // The URL keeps the choice and the rank separately, so a link restores both.
  urlParams() {
    return {
      venues: this.allChosen() ? null : this.catalog.filter((venue) => this.venues.has(venue.id)).map((venue) => venue.id).join(','),
      rank: this.minRank,
    };
  }

  // Why the search covers fewer venues, or '' when it covers all of them.
  scope() {
    const searched = this.searched().length;
    if (searched === this.catalog.length) return '';
    if (searched) return `in ${searched} of ${this.catalog.length} venues`;
    return this.venues.size ? 'because no chosen venue has this rank' : 'because no venues are selected';
  }

  // One removable filter per active topic (or the custom venue list) and the rank.
  activeFilters() {
    const filters = [...this.activeTopics].map((tag) => ({ label: this.topicLabel(tag), remove: () => this.toggleTopic(tag) }));
    if (!filters.length && !this.allChosen()) {
      const ids = [...this.venues];
      const label = !ids.length ? 'No venues' : ids.length <= 2 ? ids.join(', ') : `${ids.length} venues`;
      filters.push({ label, remove: () => this.choose(this.catalog) });
    }
    if (this.minRank) {
      const { label } = this.rankOptions().find((option) => option.rank === this.minRank);
      filters.push({ label, remove: () => this.chooseRank(null) });
    }
    return filters;
  }

  // The selection is always rebuilt from the active topics, so a venue shared by two topics
  // stays chosen while either one is on. Turning off the last topic means all venues again.
  toggleTopic(tag) {
    const active = new Set(this.activeTopics);
    if (!active.delete(tag)) active.add(tag);
    const venues = active.size ? this.catalog.filter((venue) => venue.tags.some((t) => active.has(t))) : this.catalog;
    this.choose(venues, active);
  }

  // Without explicit topics, chips light up again when the venues happen to match some.
  choose(venues, activeTopics = null) {
    this.venues = new Set(venues.map((venue) => venue.id));
    this.activeTopics = activeTopics ?? this.topicsMatchingSelection();
  }

  toggleVenues(venues, chosen) {
    venues.forEach((venue) => (chosen ? this.venues.add(venue.id) : this.venues.delete(venue.id)));
    this.activeTopics = this.topicsMatchingSelection();
  }

  chooseRank(rank) {
    this.minRank = rank;
  }

  reset() {
    this.minRank = null;
    this.choose(this.catalog);
  }

  // The topics that build exactly the chosen venues, or none for a custom selection.
  // Topics inside a broader chosen one (NLP inside ML) stay off, as a user would have left them.
  topicsMatchingSelection() {
    if (this.allChosen()) return new Set();
    const covered = this.topics().filter((tag) => this.venuesWithTag(tag).every((venue) => this.venues.has(venue.id)));
    const inside = (tag, other) => this.venuesWithTag(other).length > this.venuesWithTag(tag).length
      && this.venuesWithTag(tag).every((venue) => venue.tags.includes(other));
    const broadest = covered.filter((tag) => !covered.some((other) => inside(tag, other)));
    const union = this.catalog.filter((venue) => venue.tags.some((tag) => broadest.includes(tag)));
    return new Set(union.length === this.venues.size ? broadest : []);
  }
}
