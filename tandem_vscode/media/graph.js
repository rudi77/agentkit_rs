const api=acquireVsCodeApi(),el=id=>document.getElementById(id),svgNS='http://www.w3.org/2000/svg';
const origins={analysis:'Sprachdienst',documented:'Dokumentiert',hypothesis:'Hypothese · nicht belegt',intent:'Vereinbart',catalog:'Dateigruppierung'};
const kinds={intent:'Produktziel',step:'Planschritt',component:'Komponente',code:'Codeelement',test_candidate:'Testkandidat',patch:'Änderungsvorschlag'};
const status={idea:'Idee',clarifying:'In Klärung',ready:'Bereit',preview:'Vorschlag',applied:'Übernommen',verified:'Real geprüft',accepted:'Fachlich abgeschlossen',deferred:'Zurückgestellt',stale:'Veraltet',discarded:'Verworfen'};
let graph,layer='architecture',selected,zoom=1,list=false,representationChosen=false,lastFocus,positions=new Map();
function node(tag,text,parent){const n=document.createElement(tag);if(text!==undefined)n.textContent=text;if(parent)parent.append(n);return n;}
function vector(tag,attributes,parent){const n=document.createElementNS(svgNS,tag);for(const [name,value]of Object.entries(attributes))n.setAttribute(name,String(value));if(parent)parent.append(n);return n;}
function codeLabel(parent,value){for(const part of value.split(/([._/])/)){parent.append(document.createTextNode(part));if(/^[._/]$/.test(part))parent.append(document.createElement('wbr'));}}
function post(action,id){api.postMessage({action,layer,id,revision:graph.revision});}
function focusSelected(){[...(list?el('nodeList'):el('nodes')).querySelectorAll('[data-node-id]')].find(n=>n.dataset.nodeId===selected)?.focus({preventScroll:true});}
function choose(id,navigate=false){const hadNodeFocus=!!document.activeElement?.dataset.nodeId;selected=id;render();if(hadNodeFocus)focusSelected();const item=graph.layers[layer].nodes.find(n=>n.id===id);if(navigate&&item?.anchor)post('graphOpenNode',id);}
function setLayer(next){layer=next;selected=graph?.layers[layer]?.focus_id;el('neighborsOnly').checked=layer!=='product';zoom=1;render();}
document.querySelectorAll('[data-layer]').forEach(button=>{button.onclick=()=>setLayer(button.dataset.layer);button.onkeydown=event=>{if(!['ArrowLeft','ArrowRight'].includes(event.key))return;event.preventDefault();const values=['product','architecture','local','impact'],offset=event.key==='ArrowRight'?1:-1;setLayer(values[(values.indexOf(button.dataset.layer)+offset+4)%4]);document.querySelector(`[data-layer="${layer}"]`).focus();};});
el('filter').oninput=()=>render();el('neighborsOnly').onchange=()=>render();el('followCode').onchange=()=>{if(el('followCode').checked){selected=graph.layers[layer].focus_id;render();}};
el('zoomIn').onclick=()=>{zoom=Math.min(1.8,zoom+.15);render();};el('zoomOut').onclick=()=>{zoom=Math.max(.35,zoom-.15);render();};
el('fit').onclick=()=>{const canvas=el('canvas'),width=Number(canvas.dataset.width)||960;zoom=Math.min(1,Math.max(.6,(el('viewport').clientWidth-20)/width));render();el('viewport').scrollTo(0,0);};
el('listMode').onclick=()=>{representationChosen=true;list=!list;render();};el('openNode').onclick=()=>post('graphOpenNode',selected);el('activateStep').onclick=()=>post('graphActivateStep',selected);
window.addEventListener('resize',()=>{if(!representationChosen){list=innerWidth<650;render();}});
function visibleNodes(view){const query=el('filter').value.trim().toLocaleLowerCase();if(query)return view.nodes.filter(item=>(item.label+' '+(item.anchor?.path||'')).toLocaleLowerCase().includes(query));if(!el('neighborsOnly').checked||!selected)return view.nodes;
  const ids=new Set([selected]);for(const edge of view.edges){if(edge.from===selected)ids.add(edge.to);if(edge.to===selected)ids.add(edge.from);}return view.nodes.filter(item=>ids.has(item.id));}
function layout(view,items){const rows=new Map(),result=new Map();for(const item of items){let col=1;
  if(layer==='product')col=item.kind==='intent'?0:item.kind==='step'?1:2;
  else if(layer==='impact')col=item.id==='patch'?0:view.edges.some(edge=>edge.from==='patch'&&edge.to===item.id)?1:2;
  else if(item.id===(layer==='local'?view.focus_id:selected))col=1;
  else if(view.edges.some(edge=>edge.from===(layer==='local'?view.focus_id:selected)&&edge.to===item.id))col=2;
  else if(view.edges.some(edge=>edge.to===(layer==='local'?view.focus_id:selected)&&edge.from===item.id))col=0;
  else col=items.indexOf(item)%3;
  const row=rows.get(col)||0;rows.set(col,row+1);result.set(item.id,{x:28+col*300,y:28+row*145,col});}
  const maxRows=Math.max(1,...rows.values());return{positions:result,width:900,height:Math.max(400,28+maxRows*145)};}
function renderMap(view,items){const data=layout(view,items);positions=data.positions;const canvas=el('canvas');canvas.style.width=data.width*zoom+'px';canvas.style.height=data.height*zoom+'px';canvas.dataset.width=String(data.width);
  const nodes=el('nodes'),edges=el('edges');nodes.replaceChildren();edges.replaceChildren();nodes.style.width=data.width+'px';nodes.style.height=data.height+'px';nodes.style.transform=`scale(${zoom})`;nodes.style.transformOrigin='0 0';edges.setAttribute('width',data.width*zoom);edges.setAttribute('height',data.height*zoom);edges.setAttribute('viewBox',`0 0 ${data.width} ${data.height}`);
  const defs=vector('defs',{},edges);for(const origin of Object.keys(origins)){const marker=vector('marker',{id:'arrow-'+origin,viewBox:'0 0 10 10',refX:9,refY:5,markerWidth:6,markerHeight:6,orient:'auto-start-reverse'},defs);vector('path',{d:'M 0 0 L 10 5 L 0 10 z',class:'arrow '+origin},marker);}
  for(const edge of view.edges){const from=positions.get(edge.from),to=positions.get(edge.to);if(!from||!to)continue;const forward=to.x>from.x,x1=from.x+(forward?230:0),x2=to.x+(forward?0:230),y1=from.y+56,y2=to.y+56;
    const same=from.x===to.x,d=same?`M ${from.x+230} ${y1-15} C ${from.x+285} ${y1-15}, ${to.x+285} ${y2+15}, ${to.x+230} ${y2+15}`:`M ${x1} ${y1} C ${(x1+x2)/2} ${y1}, ${(x1+x2)/2} ${y2}, ${x2} ${y2}`;
    const path=vector('path',{d,class:'edge '+edge.origin+(edge.from===selected||edge.to===selected?' selected':''),'marker-end':`url(#arrow-${edge.origin})`},edges);vector('title',{},path).textContent=origins[edge.origin]+': '+edge.reason;
  }
  for(const item of items){const position=positions.get(item.id),button=node('button',undefined,nodes);button.className='graph-node'+(item.stale?' stale':'');button.style.left=position.x+'px';button.style.top=position.y+'px';button.dataset.nodeId=item.id;button.setAttribute('aria-pressed',String(item.id===selected));button.setAttribute('aria-label',item.label+' · '+origins[item.origin]+(item.anchor?' · Code öffnen':''));button.title=item.label+'\n'+item.reason;node('span',kinds[item.kind],button).className='node-kind';codeLabel(node('strong',undefined,button),item.label);node('small',origins[item.origin]+(item.stale?' · veraltet':''),button).className=item.origin;button.onclick=()=>choose(item.id,true);
    button.onkeydown=event=>{if(!['ArrowLeft','ArrowRight','ArrowUp','ArrowDown'].includes(event.key))return;event.preventDefault();const horizontal=event.key==='ArrowLeft'||event.key==='ArrowRight',direction=event.key==='ArrowLeft'||event.key==='ArrowUp'?-1:1;
      const candidates=items.filter(other=>other.id!==item.id).map(other=>({item:other,p:positions.get(other.id)})).filter(other=>(horizontal?other.p.x-position.x:other.p.y-position.y)*direction>0).sort((a,b)=>Math.hypot(a.p.x-position.x,a.p.y-position.y)-Math.hypot(b.p.x-position.x,b.p.y-position.y));
      if(candidates.length){choose(candidates[0].item.id);const target=[...el('nodes').children].find(n=>n.dataset.nodeId===selected);target?.focus();}
    };
  }
}
function renderInspector(view){const item=view.nodes.find(item=>item.id===selected);el('openNode').hidden=!item?.anchor;el('activateStep').hidden=!item?.step_id;el('connections').replaceChildren();
  el('nodeTitle').textContent=item?.label||'Ein Element auswählen';el('nodeOrigin').textContent=item?origins[item.origin]:'';el('nodeOrigin').className='badge '+(item?.origin||'');el('nodeReason').textContent=item?.reason||'';el('nodeAnchor').textContent=item?.anchor?`${item.anchor.path}:${item.anchor.line}`:'';el('nodeState').textContent=item?(item.stale?'Veralteter Quellenstand · neu prüfen.':item.status?status[item.status]||item.status:''):'';
  if(!item)return;for(const edge of view.edges.filter(edge=>edge.from===item.id||edge.to===item.id)){const other=view.nodes.find(n=>n.id===(edge.from===item.id?edge.to:edge.from));if(!other)continue;const article=node('article',undefined,el('connections')),button=node('button',(edge.from===item.id?'→ ':'← ')+other.label,article);button.onclick=()=>choose(other.id,true);node('small',origins[edge.origin],article).className='badge '+edge.origin;node('p',edge.reason,article);}
}
function render(){if(!graph)return;const view=graph.layers[layer];if(!view.nodes.some(item=>item.id===selected))selected=view.focus_id||view.nodes[0]?.id;
  document.querySelectorAll('[data-layer]').forEach(button=>{if(button.dataset.layer===layer)button.setAttribute('aria-current','page');else button.removeAttribute('aria-current');});
  el('codeFocus').textContent=graph.context?`${graph.context.focus.path}:${graph.context.focus.line} · ${graph.context.focus.name}`:'Noch kein Codefokus';el('coverage').textContent=view.notes.join('\n');
  const items=visibleNodes(view);el('empty').hidden=items.length>0;el('empty').textContent=view.nodes.length?'Keine Elemente passen zum Suchfilter.':'Dieser Ausschnitt enthält noch keine belegten Elemente.';el('viewport').hidden=list||!items.length;el('nodeList').hidden=!list||!items.length;el('listMode').setAttribute('aria-pressed',String(list));el('listMode').textContent=list?'Grafische Ansicht':'Listenansicht';
  renderMap(view,items);el('nodeList').replaceChildren();for(const item of items){const li=node('li',undefined,el('nodeList')),button=node('button',item.label,li);button.dataset.nodeId=item.id;button.setAttribute('aria-pressed',String(item.id===selected));node('small',origins[item.origin]+(item.anchor?' · '+item.anchor.path+':'+item.anchor.line:''),button).className=item.origin;button.onclick=()=>choose(item.id,true);}
  el('mapCount').textContent=`${items.length} von ${view.nodes.length} Elementen dieses Ausschnitts · Karte unvollständig`;renderInspector(view);
}
window.addEventListener('message',({data})=>{if(data.type==='error'){el('mapError').textContent=data.text;el('mapError').hidden=false;return;}if(data.type!=='graphState')return;const changed=graph?.revision!==data.graph.revision;graph=data.graph;
  const focusKey=graph.context?.focus.uri+':'+graph.context?.focus.line;if(el('followCode').checked&&focusKey!==lastFocus)selected=graph.layers[layer].focus_id;lastFocus=focusKey;
  if(!representationChosen)list=innerWidth<650;if(changed){const hadNodeFocus=!!document.activeElement?.dataset.nodeId;el('mapError').hidden=true;render();if(hadNodeFocus)focusSelected();}});
api.postMessage({action:'graphReady'});
