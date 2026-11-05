export interface ArchitectureNode {id:string;label:string;path:string;line:number}
export interface ArchitectureEdge {from:string;to:string;kind:'documented'|'hypothesis';reason:string}
export interface Architecture {nodes:ArchitectureNode[];edges:ArchitectureEdge[];source_uri:string;root_uri:string;sha256:string;imported_at:string;stale:boolean}
export function validateArchitecture(input:unknown):Pick<Architecture,'nodes'|'edges'> {
  const value=input as Architecture;if(!value||!Array.isArray(value.nodes)||!Array.isArray(value.edges)||value.nodes.length>200||value.edges.length>500)throw new Error('Die Architekturdatei benötigt nodes (maximal 200) und edges (maximal 500).');
  const ids=new Set<string>();
  for(const node of value.nodes){if(!node||typeof node.id!=='string'||!node.id||ids.has(node.id)||typeof node.label!=='string'||!node.label.trim()||typeof node.path!=='string'||!node.path||/^[\\/]|:|(^|[\\/])\.\.([\\/]|$)/.test(node.path)||!Number.isSafeInteger(node.line)||node.line<1)throw new Error('Jeder Architekturknoten benötigt eindeutige ID, Bezeichnung und einen relativen Codeanker mit Zeile.');ids.add(node.id);}
  for(const edge of value.edges){if(!edge||!ids.has(edge.from)||!ids.has(edge.to)||!['documented','hypothesis'].includes(edge.kind)||typeof edge.reason!=='string'||!edge.reason.trim())throw new Error('Beziehungen benötigen existierende Knoten, documented/hypothesis und eine Begründung.');}
  return{nodes:structuredClone(value.nodes),edges:structuredClone(value.edges)};
}
