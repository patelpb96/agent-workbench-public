// Compile-time checks of the agreed internal seam, not new wire declarations.
// No runtime code or substitute implementation is emitted.
import type { LayoutModule, WorkspaceController, WorkspaceDependencies } from './workspace-ports.js';
type Assert<T extends true> = T;
type LayoutExports = typeof import('./layout.js');
type ControllerExports = typeof import('./controller.js');
export type LayoutApiCheck = Assert<LayoutExports extends LayoutModule ? true : false>;
export type ControllerApiCheck = Assert<ControllerExports['createWorkspaceController'] extends
  (deps: WorkspaceDependencies) => WorkspaceController ? true : false>;
