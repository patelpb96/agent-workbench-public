// Tauri wiring kept apart from the pure, unit-tested client.
import { invoke, isTauri } from '@tauri-apps/api/core';
import { createInspectorClient } from './client.js';

export const inspectorAvailable = (): boolean => isTauri();
export const inspectorClient = createInspectorClient((command, args) =>
  isTauri() ? invoke(command, args) : Promise.reject({ code: 'desktop_only', user_message: 'The inspector is available in the desktop app.' }));
