import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const command = <T>(name: string, args?: Record<string, unknown>): Promise<T> =>
  invoke<T>(name, args);

export const onEvent = <T>(
  name: string,
  handler: (payload: T) => void,
): Promise<UnlistenFn> => listen<T>(name, (event) => handler(event.payload));

export function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  if (Array.isArray(error)) {
    return error
      .map((item) =>
        typeof item === "object" && item && "message" in item
          ? String(item.message)
          : String(item),
      )
      .join("\n");
  }
  return "処理を完了できませんでした。";
}
