import type { GraphEdge, GraphLayer, GraphNode, SoftwareGraph } from './software-graph';

export interface GraphSelection {layer:GraphLayer;kind:'node'|'edge';id?:string;edge_index?:number;revision:string}
export interface VisualFocus {
  selection:GraphSelection;label:string;origin:GraphNode['origin'];reason:string;nodes:GraphNode[];edge?:GraphEdge;
  code_binding?:{uri:string;sha256:string};stale:boolean;
  files?:{uri:string;sha256:string}[];
}
export const originLabel:Record<GraphNode['origin'],string>={analysis:'Sprachdienst',documented:'Dokumentiert',hypothesis:'Hypothese · nicht belegt',intent:'Vereinbart',catalog:'Dateigruppierung'};

// Aus der Webview kommen ausschließlich IDs und eine Revision. Beschriftungen,
// Herkunft und Quellanker werden aus der tatsächlichen Host-Karte aufgelöst.
export function resolveVisualFocus(graph:SoftwareGraph,selection:GraphSelection):VisualFocus {
  if(!selection||!['product','architecture','local','impact'].includes(selection.layer)||!['node','edge'].includes(selection.kind))throw new Error('Unbekannte Kartenauswahl.');
  if(selection.revision!==graph.revision)throw new Error('Der Kartenausschnitt wurde aktualisiert. Bitte erneut auswählen.');
  const view=graph.layers[selection.layer];let nodes:GraphNode[],edge:GraphEdge|undefined;
  if(selection.kind==='node')nodes=view.nodes.filter(node=>node.id===selection.id);
  else{
    if(!Number.isSafeInteger(selection.edge_index)||(selection.edge_index??-1)<0)throw new Error('Diese Verbindung existiert nicht.');
    edge=view.edges[selection.edge_index!];if(!edge)throw new Error('Diese Verbindung existiert nicht.');
    nodes=[view.nodes.find(node=>node.id===edge!.from)!,view.nodes.find(node=>node.id===edge!.to)!];
  }
  if(!nodes.length||nodes.some(node=>!node))throw new Error('Dieses Kartenelement existiert nicht.');
  const label=edge?nodes[0].label+' → '+nodes[1].label:nodes[0].label;
  const focus:VisualFocus={selection:structuredClone(selection),label:label.length>400?label.slice(0,397)+'…':label,
    origin:edge?.origin??nodes[0].origin,reason:edge?.reason??nodes[0].reason,nodes:structuredClone(nodes),edge:edge?structuredClone(edge):undefined,stale:nodes.some(node=>node.stale===true)};
  if(['local','impact'].includes(selection.layer)&&graph.context){focus.code_binding={uri:graph.context.focus.uri,sha256:graph.context.sha256};if(graph.context.pending)focus.stale=true;}
  return focus;
}

export function visualFocusCurrent(focus:VisualFocus,graph:SoftwareGraph):boolean {
  const view=graph.layers[focus.selection.layer];
  if(focus.stale||focus.nodes.some(saved=>{
    const current=view.nodes.find(node=>node.id===saved.id);
    // Ein Sprachdienst-Ausschnitt kann durch Navigation wechseln. Der gewählte
    // Bezug bleibt bei unveränderten, im Host gebundenen Dateien erhalten.
    if(!current&&focus.selection.layer==='local'&&focus.files?.length)return false;
    return saved.stale||JSON.stringify(current)!==JSON.stringify(saved);
  }))return false;
  if(focus.edge&&!view.edges.some(edge=>JSON.stringify(edge)===JSON.stringify(focus.edge))&&!(focus.selection.layer==='local'&&focus.files?.length))return false;
  if(focus.files?.some(file=>graph.context?.focus.uri===file.uri&&graph.context.sha256!==file.sha256))return false;
  if(!focus.files?.length&&focus.code_binding&&(graph.context?.pending||graph.context?.focus.uri!==focus.code_binding.uri||graph.context.sha256!==focus.code_binding.sha256))return false;
  return true;
}

export function visualFocusPrompt(focus:VisualFocus):string {
  if(focus.stale)throw new Error('Der visuelle Bezug ist veraltet. Bitte in der Karte erneut auswählen oder den Bezug im Chat entfernen.');
  return '\n\nVisuell ausgewählter Gesprächsbezug (Lesekontext; keine neue Codeauswahl oder Schreibfreigabe):\n'+JSON.stringify({
    kind:focus.selection.kind,layer:focus.selection.layer,label:focus.label.slice(0,500),origin:focus.origin,origin_label:originLabel[focus.origin],reason:focus.reason.slice(0,2000),
    nodes:focus.nodes.map(node=>({label:node.label.slice(0,500),kind:node.kind,origin:node.origin,reason:node.reason.slice(0,1000),anchor:node.anchor})),
    edge:focus.edge?{from:focus.nodes[0].label.slice(0,500),to:focus.nodes[1].label.slice(0,500),origin:focus.edge.origin}:undefined,
    limitation:'Hypothesen sind keine bestätigten Abhängigkeiten. Die Karte ist ein Ausschnitt. Beziehe die Frage auf diese Auswahl und prüfe den aktuellen Code bei Bedarf lesend.'});
}
