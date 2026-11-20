// Der Nutzereingabe bleibt ein eigenes Budget. Visueller Bezug, Contract und
// Repository-Aufgabe dürfen dieses nicht bereits vor dem Senden verbrauchen.
export const MAX_CHAT_BYTES=256_000;
export const MAX_HISTORY_BYTES=320_000;
export function assertChatInput(prompt:string):void {
  const bytes=Buffer.byteLength(prompt,'utf8');
  if(bytes>MAX_CHAT_BYTES)throw new Error(`Deine Nachricht enthält ${Math.ceil(bytes/1000)} KB. Pro Nachricht sind bis zu 256 KB möglich. Teile größere Logs auf mehrere Nachrichten auf; dein Text bleibt im Eingabefeld erhalten.`);
}
