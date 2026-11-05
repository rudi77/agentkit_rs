// Browserprüfung der echten Webview-Dateien. Die API-Gegenstelle ist ein
// lokaler Zustandsautomat; weder Repository- noch Modellzugriffe finden statt.
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const http=require('node:http');
const {chromium}=require(process.env.TANDEM_PLAYWRIGHT_MODULE||'playwright');
const {workbenchHtml}=require('../out/workbench-html');
const root=path.resolve(__dirname,'..');
const results=path.join(root,'test-results/ui');fs.mkdirSync(results,{recursive:true});
const source={id:'src1',path:'src/Service.cs',sha256:'a'.repeat(64),start_line:10,end_line:28,kind:'code',editor:false};
const proposal={goal:'Historische Records dauerhaft löschen',not_now:'Keine Löschung in PROD. Kein Soft-Delete-Fallback.',open_questions:'Welcher SBF-Endpunkt garantiert dauerhafte Löschung innerhalb der gewählten OrgUnit?',steps:[{id:'contract',title:'Löschvertrag und Scope klären',why:'Der aktuelle Code und die vorhandenen Tests erwarten HTTP 405. Der gewünschte Hard-Delete-Vertrag muss konkret festgelegt werden.',files:['src/Service.cs'],acceptance:'GUID, OrgUnit und Fehlersemantik sind gemeinsam festgelegt.',source_ids:['src1']},{id:'adapter',title:'Hard-Delete gezielt anbinden',why:'Den vereinbarten Endpunkt mit unverändertem Tenant-Scope aufrufen.',files:['src/Service.cs'],acceptance:'Ein gezielter Test belegt den richtigen Aufruf.',source_ids:['src1']}]};
const analysis={answer:'## Ergebnis\n\nDer aktuelle Pfad liefert **HTTP 405**.\n\n- `DeleteRecord` lehnt historische Einzel-Löschungen ab.\n- Der gewünschte Hard-Delete-Pfad muss noch festgelegt werden.\n\n<script>window.injected=true</script>',suggested_plan:proposal,sources:[source],instruction_paths:['AGENTS.md'],catalog_id:'catalog',stale:false,warnings:[],plan_revision:1,root_uri:'file:///fixture',task:'Hard-Delete statt HTTP 405',created_at:'2026-11-03T12:00:00Z',overlays:[]};
const initial={plan:{revision:1,goal:'Gewünschtes Verhalten klären',not_now:'Keine Nebenarbeiten',acceptance:''},chat:[],audit:[]};
const shim=`window.messages=[];window.testState=${JSON.stringify(initial)};
window.deliver=()=>window.dispatchEvent(new MessageEvent('message',{data:{type:'state',state:structuredClone(window.testState),provider:'azure',busy:false,verifying:false,stream:'',error:''}}));
window.acquireVsCodeApi=()=>({postMessage:m=>{window.messages.push(m);if(m.action==='ready')setTimeout(window.deliver,0);}});`;
const server=http.createServer((req,res)=>{
  if(req.url==='/') {
    const html=workbenchHtml('/media/workbench.js','/media/workbench.css','test-nonce',"'self'").replace('<script nonce="test-nonce" src=',`<script nonce="test-nonce">${shim}</script><script nonce="test-nonce" src=`);
    res.setHeader('Content-Type','text/html; charset=utf-8');res.end(html);
  } else if(['/media/workbench.js','/media/workbench.css'].includes(req.url)) {res.setHeader('Content-Type',req.url.endsWith('.js')?'text/javascript':'text/css');res.end(fs.readFileSync(path.join(root,req.url)));}
  else {res.writeHead(404);res.end();}
});
async function main(){
  await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
  const installed=process.env.TANDEM_CHROMIUM||(process.platform==='win32'&&fs.existsSync('C:/Program Files/Google/Chrome/Application/chrome.exe')?'C:/Program Files/Google/Chrome/Application/chrome.exe':undefined);
  const browser=await chromium.launch({headless:true,...(installed?{executablePath:installed}:{} )});
  const page=await browser.newPage({viewport:{width:430,height:960},deviceScaleFactor:1});
  const errors=[];page.on('pageerror',error=>errors.push(error.message));
  try {
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForFunction(()=>document.querySelector('#provider').textContent.includes('azure'));
    assert.equal(await page.locator('.page:visible').count(),1);
    assert.equal(await page.locator('.primary:visible').count(),1);
    await page.screenshot({path:path.join(results,'01-start.png'),fullPage:true});
    await page.locator('#repositoryTask').fill('Hard-Delete statt HTTP 405');
    await page.locator('#exploreRepository').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'exploreRepository');
    await page.evaluate(a=>{testState.analysisTask=a.task;testState.exploration={...a,suggested_plan:null,created_at:'2026-11-03T11:59:00Z',answer:'Welche **OrgUnit** soll der Löschvertrag abdecken?'};deliver();},analysis);
    await page.locator('#page-plan').waitFor({state:'visible'});
    assert.equal(await page.locator('#planNoProposalAnswer').isVisible(),true);
    assert.equal(await page.locator('#planEmpty').isVisible(),false);
    await page.locator('#planMessage').fill('Nur die ausdrücklich gewählte OrgUnit.');
    await page.locator('#discussPlan').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'discussRepository');
    await page.evaluate(a=>{testState.analysisTask=a.task;testState.exploration=a;deliver();},analysis);
    await page.locator('#page-plan').waitFor({state:'visible'});
    assert.equal(await page.locator('#planSteps .step-card').count(),2);
    assert.equal(await page.locator('#planEditor').isVisible(),false);
    assert.equal(await page.locator('#analysis').isVisible(),false);
    await page.screenshot({path:path.join(results,'02-plan.png'),fullPage:true});
    await page.locator('#analysisDetail>summary').click();
    assert.equal(await page.locator('#analysis strong').innerText(),'HTTP 405');
    assert.equal(await page.evaluate(()=>!!window.injected),false);
    assert.equal(await page.locator('#analysis script').count(),0);
    await page.locator('#analysisDetail>summary').click();
    await page.locator('#planPrimary').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'adoptChangePlan');
    await page.evaluate(p=>{testState.changePlan={...p,revision:1,stale:false,root_uri:'file:///fixture',catalog_id:'catalog',sources:[],basis_created_at:testState.exploration.created_at};deliver();},proposal);
    await page.locator('#planPrimary').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).id),'contract');
    await page.evaluate(()=>{testState.activePlanStepId='contract';testState.plan={revision:2,goal:'Löschvertrag und Scope klären',not_now:'Keine PROD-Löschung',acceptance:'Scope klären'};deliver();});
    await page.locator('#page-work').waitFor({state:'visible'});
    assert.equal(await page.locator('#discussWork').isVisible(),true);
    assert.equal(await page.locator('#chooseCode').getAttribute('open'),null);
    await page.screenshot({path:path.join(results,'03-klaeren.png'),fullPage:true});
    await page.locator('#prompt').fill('Hard-Delete ist fachlich entschieden. Kläre nur noch den Endpoint.');
    await page.locator('#discussWork').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'discussRepository');
    await page.evaluate(()=>{testState.scope={uri:'file:///fixture/src/Service.cs',path:'src/Service.cs',prefix:'// Header\n',editable:'return input;',suffix:'',version:1};deliver();});
    await page.locator('#proposePatch').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).mode),'pair');
    await page.evaluate(()=>{testState.proposal={status:'ready',patch:{reason:'Die vereinbarte Änderung bleibt auf **diesen Bereich** begrenzt.'},base:testState.scope};deliver();});
    await page.locator('#preview').click();
    await page.locator('#apply').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'apply');
    await page.evaluate(()=>{testState.proposal.status='applied';deliver();});
    assert.equal(await page.locator('#verificationBox').isVisible(),true);
    await page.locator('#verify').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'verify');
    for(const width of [320,760,1250]) {await page.setViewportSize({width,height:960});assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),'Keine horizontale Überbreite bei '+width);}
    await page.locator('[data-page=plan]').first().click();
    await page.locator('#togglePlanEdit').click();
    await page.locator('#changeGoal').fill('Menschlich angepasster Plan');
    await page.evaluate(()=>deliver());
    assert.equal(await page.locator('#changeGoal').inputValue(),'Menschlich angepasster Plan');
    assert.equal(await page.locator('#planPrimary').isVisible(),false);
    await page.locator('#cancelPlanEdit').click();
    assert.equal(await page.locator('#planPrimary').isVisible(),true);
    await page.evaluate(()=>{testState.exploration.created_at='2026-11-03T12:01:00Z';testState.exploration.suggested_plan.goal='Neuer Vorschlag des Agents';deliver();});
    assert.equal(await page.locator('#keepPlan').isVisible(),true);
    await page.locator('#keepPlan').click();
    assert.equal(await page.locator('#planPrimary').innerText(),'An diesem Schritt arbeiten');
    assert.equal(await page.evaluate(()=>testState.changePlan.goal),'Historische Records dauerhaft löschen');
    await page.locator('[data-page=work]').first().click();
    await page.evaluate(()=>{testState.changePlan.stale=true;testState.exploration.stale=true;deliver();});
    assert.equal(await page.locator('#workStale').isVisible(),true);
    assert.equal(await page.locator('#proposePatch').isVisible(),false);
    await page.locator('#revalidateWork').click();
    assert.equal(await page.evaluate(()=>messages.at(-1).action),'revalidateAnalysis');
    await page.evaluate(()=>{testState.changePlan.stale=false;testState.exploration.stale=false;deliver();});
    assert.equal(await page.locator('#workContent').isVisible(),true);
    assert.deepEqual(errors,[]);
    console.log('PASS: geführter Ablauf von Aufgabe über Plan und Klärung bis Diff/Übernahme/Prüfung; sichere Markdown-Darstellung, erhaltene Eingaben und responsive Breiten.');
    console.log('Screenshots:',results);
  } finally {await browser.close();server.close();}
}
main().catch(error=>{console.error(error);server.close();process.exitCode=1;});
