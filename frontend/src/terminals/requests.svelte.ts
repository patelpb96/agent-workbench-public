// Requests from elsewhere in the app (the Files panel) for the Terminals tab to act on when it is showing: open a new
// terminal in a folder, or fill the Launch agent row's folder. Each request is taken once.
class TerminalRequests {
  openHere = $state<string | null>(null);
  launchHere = $state<string | null>(null);
  takeOpen(): string | null {
    const folder = this.openHere;
    this.openHere = null;
    return folder;
  }
  takeLaunch(): string | null {
    const folder = this.launchHere;
    this.launchHere = null;
    return folder;
  }
}

export const terminalRequests = new TerminalRequests();
