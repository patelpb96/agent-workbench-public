// The terminal the person last clicked or typed in (a pane on the Terminals tab or a terminal window on the
// Canvas). The Files panel's "Insert path" types into it.
class LastFocused {
  id = $state<string | null>(null);
  label = $state('');
  set(id: string, label: string): void {
    this.id = id;
    this.label = label;
  }
}

export const lastFocused = new LastFocused();
