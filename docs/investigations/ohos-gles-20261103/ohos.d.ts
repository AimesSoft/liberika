export const nativeCreate: (mode: number, headroom: number, upscaler: number, alpha: number) => number;
export const nativeLastError: () => string;
export const nativeDestroy: (id: number) => void;
export const nativeInvoke: (id: number, method: string, args: string) => string;
export const nativeRenderTick: (id: number, time: number) => string;
export const nativePollEvent: (id: number) => string | null;
export const nativeDetachSurface: (id: number) => number;
export const nativeResizeSurface: (id: number, width: number, height: number, scale: number) => number;
export const nativeAttachSurface: (id: number, surfaceId: bigint, width: number, height: number, scale: number) => number;

export const nativeInvokeAsync: (id: number, method: string, args: string) => Promise<string>;
export const nativeStartFrames: (id: number, callback: (event: string) => void) => void;
export const nativeStopFrames: (id: number) => void;
export const nativeLoadUpscalerModelAsync: (id: number, name: string, bytes: Uint8Array) => Promise<number>;
