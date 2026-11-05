export interface WorkspaceSnapshot {fingerprint:string;files:Record<string,string>;bytes:number;coverage:string}
export interface WorkspaceEvidence {root_uri:string;before_hash:string;after_hash?:string;file_count:number;byte_count:number;changed_paths:string[];coverage:string}
export function changedWorkspaceFiles(before:WorkspaceSnapshot,after:WorkspaceSnapshot):string[] {
  return [...new Set([...Object.keys(before.files),...Object.keys(after.files)])].filter(path=>!Object.hasOwn(before.files,path)||!Object.hasOwn(after.files,path)||before.files[path]!==after.files[path]).sort();
}
