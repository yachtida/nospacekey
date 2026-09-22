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
  if (typeof error === "object" && error && "kind" in error) {
    switch (error.kind) {
      case "InvalidEncoding": return "文字コードを正しく読み取れないため、辞書は変更しませんでした。UTF-8 または UTF-16 で保存し直してから取り込んでください。";
      case "NotFound": return "対象の単語が見つかりません。一覧を更新してから再試行してください。";
      case "Duplicate": return "同じ読みと単語がすでに登録されています。";
      case "Invalid": return "field" in error && error.field === "ruby" ? "読みをひらがな・カタカナで入力してください。" : "単語の長さや改行・制御文字を確認してください。";
      case "Unreadable": return "辞書を読み取れませんでした。ファイルのアクセス権を確認してください。";
      case "QuarantineFailed": return "破損した辞書を安全に退避できないため、変更を停止しました。";
      case "Io": return "message" in error ? String(error.message) : "ファイルを読み書きできませんでした。";
    }
  }
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
