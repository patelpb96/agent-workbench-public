import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
export type ProviderId = 'claude' | 'codex' | { other: string };
export type SupportStatus = 'supported' | 'unsupported' | 'unknown';
export type CapabilityName = 'structured_output' | 'streaming_input' | 'resume' | 'cancel' | 'approval_events' | 'usage_reporting';
export interface RuntimeInfo { interface_version: 1; app_version: string; os: string; providers: { provider: ProviderId; executable_found: boolean; version?: string; capabilities: {name: CapabilityName; status: SupportStatus; source: string; checked_at?: string; version?: string}[] }[] }
export type BackgroundKind = 'solid' | 'transparent' | 'image' | 'video';
export interface WindowAppearance { background_kind: BackgroundKind; color_rgba: string; window_alpha: number }
export interface SelectedBackground { status: 'selected' | 'cancelled'; asset_id?: string; asset_url?: string; kind?: 'image' | 'video'; display_name?: string; mime_type?: string; width?: number; height?: number; duration_ms?: number }
export interface AppError { code: string; user_message: string; recoverable: boolean }
export const native = isTauri();
async function command<T>(name: string, args?: Record<string, unknown>): Promise<T> {
  if (!native) throw new Error('Browser preview: native window and file access unavailable.');
  try {
    return await invoke<T>(name, args);
  } catch (error) {
    if (error && typeof error === 'object' && 'user_message' in error) {
      throw new Error(String((error as AppError).user_message));
    }
    throw error;
  }
}
export const runtimeInfo = () => command<RuntimeInfo>('get_runtime_info');
export const ownerName = async (): Promise<string> => {
  try { return await invoke<string>('owner_name'); } catch { return ''; }
};
export const resetWindow = () => command<void>('reset_window');
export const setAppearance = (appearance: WindowAppearance) => command<void>('set_window_appearance', { appearance });
export const openBackground = (kind: 'image' | 'video') => command<SelectedBackground>('open_background_file', { kind });
export const visibilityChanged = (callback: (visible: boolean) => void) => native ? listen<{is_visible: boolean}>('window_visibility_changed', event => callback(event.payload.is_visible)) : Promise.resolve(() => {});
// Only accept a backend-issued URL for the selected file; never derive one from a path.
export function selectedAssetUrl(asset: SelectedBackground): string {
  if (asset.status !== 'selected' || !asset.asset_id || !asset.asset_url) throw new Error('Backend did not return a selected media URL.');
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(asset.asset_id)) throw new Error('Invalid media asset ID.');
  const url = new URL(asset.asset_url);
  if (url.origin !== 'http://background.localhost' || url.pathname !== `/${asset.asset_id}` || url.port || url.username || url.password || url.search || url.hash) throw new Error('Unexpected media asset URL.');
  return url.href;
}
