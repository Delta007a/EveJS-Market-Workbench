(() => {
  'use strict';
  const API = '/api/v1';
  const $ = id => document.getElementById(id);
  const state = { preset:'GENERAL_TQ', policy:null, loadedId:null, dirty:false, preview:null, previewMap:new Map(), previewCurrent:false,
    catalog:[], catalogTotal:0, page:0, pageSize:50, selected:null, detail:null, pageName:'dashboard', schema:null, tree:null,
    candidates:[], audits:new Map(), editor:null, loadSerial:0, catalogSerial:0, displayName:'', description:'', author:'', importDraft:null, importSerial:0, importCreating:false, isNew:false, quick:null, openScopes:new Set(), busy:false, loadingPreset:true, revision:0, distribution:null, distributionSchema:null, distributionPreview:null, stationNames:new Map(), blueprintSchema:null, blueprintData:null, blueprintPage:0, blueprintSerial:0 };
  const labels = {tq_snapshot:'TQ Market Prices',tq_snapshot_sell_fallback:'TQ Sell reference',tq_snapshot_buy_fallback:'TQ Buy reference',
    tq_average_price:'TQ average',funded_cost:'Production Cost',npc_acquisition:'NPC Acquisition Reference',t1_variant_sell:'T1 family Sell price',t1_variant_buy:'T1 family Buy price',core_manifest_cost:'Preset resource reference',captured_market:'Captured market reference',
    npc_min_sell:'NPC minimum Sell',bpo_npc_or_base:'Blueprint reference price',rare_reference:'Item reference price',
    skillbook_ladder:'Skillbook reference price',command_center_ladder:'Command center reference price'};
  const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  const number = value => value == null || value === '' ? '—' : typeof value === 'number' ? new Intl.NumberFormat('en-US',{maximumFractionDigits:2}).format(value) : String(value);
  const price = value => value == null ? '—' : Number(value).toLocaleString('en-US',{minimumFractionDigits:2,maximumFractionDigits:2});
  const snapshotLabel = aggregation => aggregation==='jita_snapshot_history_50_50'?'Jita + trading history · 50/50':aggregation?.startsWith('jita_')?'Jita · highest eligible Buy / lowest Sell':'Historical five-hub median';
  const title = value => String(value ?? '').replaceAll('_',' ').replace(/\b\w/g,x=>x.toUpperCase());
  const dataId = item => item?.type_id ?? item?.typeID ?? item?.id;
  const api = async (path, options={}) => {
    const response = await fetch(API+path,{headers:{'Content-Type':'application/json'},...options});
    const body = await response.json().catch(()=>({}));
    if(!response.ok) throw new Error(body.error || body.message || `Request failed (${response.status})`);
    return body;
  };
  const post = (path,body) => api(path,{method:'POST',body:JSON.stringify(body)});
  function notice(message,error=false){$('notice').textContent=message;$('notice').classList.remove('hidden');$('notice').classList.toggle('error',error);}
  function metric(label,value,alert=false){return `<div class="metric ${alert?'alert':''}"><strong>${esc(number(value))}</strong><span>${esc(label)}</span></div>`;}
  function builtIn(){return state.loadedId===state.preset;}
  function presetLabel(id){return id==='GENERAL_TQ'?'TQ-like Market':id==='LEGACY_V1'?'Legacy Market':id;}
  function availabilityLabel(value){return ({unseeded:'Not on market',buy_sell:'Buy + Sell',sell_only:'Sell only',buy_only:'Buy only',preset:'Keep preset availability'})[value]||value;}
  function refreshChrome(){
    $('duplicateBtn').disabled=state.loadingPreset||state.busy;
    $('policyName').textContent=state.displayName||presetLabel(state.loadedId)||'Loading…';
    $('loadPresetBtn').textContent=state.dirty?'Discard changes':'Reload';
    $('dirtyBadge').textContent=builtIn()?'Built-in Preset':state.dirty?'Unsaved changes':'Saved';
    $('dirtyBadge').className='badge '+(state.dirty?'warn':'ok');
    $('heroPolicy').textContent=state.displayName||presetLabel(state.loadedId)||'Loading…';$('heroMode').textContent=presetLabel(state.preset);
    $('heroState').textContent=builtIn()?'Start here, then Edit a Copy to make it yours.':state.dirty?'Changes are in your draft. Review and save when ready.':'Your preset is saved. Review it before building.';
    $('saveBtn').classList.toggle('hidden',builtIn());$('saveWorkflow').disabled=builtIn()||state.loadingPreset||state.busy;
    for(const id of ['reviewWorkflow','dashboardPreview','loadSavedBtn','loadPresetBtn','preset','welcomeTq','welcomeLegacy','welcomeSaved','exportPresetBtn','importPresetBtn'])$(id).disabled=state.loadingPreset||state.busy;
    $('saveBtn').disabled=state.busy||state.loadingPreset;
    $('previewBtn').disabled=state.busy||state.loadingPreset;
    const ready=state.loadedId&&!state.dirty&&!builtIn()&&state.distributionPreview?.valid&&state.distributionPreview?.item_policy_buildable!==false;$('buildCandidateBtn').disabled=!ready;
    $('buildReadiness').textContent=ready?'Saved preset ready to build':'Save your preset and Preview Distribution before building';
    $('candidateHint').textContent=ready?'Build creates a separate database under Workbench storage.':'Save a custom preset before building. Built-in presets stay read-only.';
    refreshDistributionControls();refreshBlueprintControls();
    for(const id of ['createRuleBtn','createProfileBtn','editOverrideBtn'])$(id).disabled=builtIn()||state.loadingPreset||state.busy;
  }
  function markDirty(){state.revision++;state.dirty=true;state.previewCurrent=false;state.distributionPreview=null;renderDistributionPreview();refreshChrome();renderRules();renderProfiles();}
  function setPolicy(policy,id,preset,displayName,distribution,metadata={}){
    state.revision++;state.description=metadata.description||'';state.author=metadata.author||'';
    state.policy=structuredClone(policy);state.loadedId=id;state.preset=preset;state.displayName=displayName||presetLabel(id);state.isNew=false;state.quick=null;state.dirty=false;state.preview=null;state.previewMap=new Map();state.previewCurrent=false;
    state.distribution=structuredClone(distribution||state.distributionSchema?.default);state.distributionPreview=null;renderDistribution();
    $('preset').value=preset;refreshChrome();renderRules();renderProfiles();renderDashboard();renderUnresolved();
  }
  async function loadSchema(){
    try{state.schema=await api('/policies/schema');$('factOptions').innerHTML=(state.schema.facts||[]).map(x=>`<option value="${esc(x)}"></option>`).join('');
      $('factFilter').innerHTML='<option value="">All tags</option>'+(state.schema.facts||[]).map(x=>`<option value="${esc(x)}">${esc(title(x))}</option>`).join('');
      renderSources();await loadBlueprintSchema();
    }catch(error){notice(`Policy schema unavailable: ${error.message}`,true);}
  }
  function renderSources(){
    const sourceList=state.preset==='GENERAL_TQ'?(state.schema?.general_tq_price_sources||[]):(state.schema?.sources||[]);
    for(const id of ['sellSource','buySource']){const sel=$(id),old=sel.value;sel.innerHTML=sourceList.map(x=>`<option value="${esc(x)}">${esc(labels[x]||title(x))}</option>`).join('');if(sourceList.includes(old))sel.value=old;}
  }
  async function loadPreset(id=$('preset').value){
    const serial=++state.loadSerial;
    state.loadingPreset=true;refreshChrome();
    notice(`Loading ${presetLabel(id)}…`);
    try{const response=await api('/policies/'+encodeURIComponent(id));if(serial!==state.loadSerial)return;
      const mode=response.preset==='LEGACY_V1'||response.preset==='GENERAL_TQ'?response.preset:id;
      setPolicy(response.policy,id,mode,response.display_name,response.distribution,response);renderSources();
      await Promise.all([loadTree(),loadCatalog(),refreshCandidates(),loadDashboardPreview(serial),inspectQuick()]);
      if(serial===state.loadSerial){state.loadingPreset=false;refreshChrome();notice(`${presetLabel(id)} loaded. Use Edit a Copy to customize this market.`);}
    }catch(error){state.loadingPreset=false;refreshChrome();if(serial===state.loadSerial)notice(`Could not load preset: ${error.message}`,true);}
  }
  async function loadSaved(id){
    const serial=++state.loadSerial;
    state.loadingPreset=true;refreshChrome();
    try{const response=await api('/policies/'+encodeURIComponent(id));if(serial!==state.loadSerial)return;
      const mode=response.preset==='LEGACY_V1'?'LEGACY_V1':'GENERAL_TQ';setPolicy(response.policy,id,mode,response.display_name,response.distribution,response);renderSources();
      await Promise.all([loadTree(),loadCatalog(),refreshCandidates(),loadDashboardPreview(serial),inspectQuick()]);
      state.loadingPreset=false;refreshChrome();$('savedDialog').close();notice(`Loaded ${state.displayName}.`);
    }catch(error){state.loadingPreset=false;refreshChrome();notice(`Could not open saved preset: ${error.message}`,true);}
  }
  async function loadDashboardPreview(serial){
    const revision=state.revision;
    $('dashboardCards').innerHTML='<div class="loading-card">Resolving policy coverage…</div>';
    try{const response=await post('/preview',{policy:state.policy,preset:state.preset});if(serial!==state.loadSerial||revision!==state.revision)return;
      acceptPreview(response);renderDashboard();renderUnresolved();renderRules();loadCatalog();loadSnapshotIdentity();
    }catch(error){if(serial===state.loadSerial){$('dashboardCards').innerHTML='<div class="loading-card">Coverage unavailable. Use Preview changes to retry.</div>';notice(`Preview failed: ${error.message}`,true);}}
  }
  function acceptPreview(response){
    state.preview=response;state.previewMap=new Map((response.items||[]).map(item=>[String(dataId(item)),item]));state.previewCurrent=true;
    const first=response.items?.find(x=>x.tq?.captured_at);
    $('snapshotInfo').textContent=first?.tq?.captured_at ? `${new Date(first.tq.captured_at).toLocaleString()} · ${snapshotLabel(first.tq.aggregation)}` : 'No TQ capture timestamp for this policy';
  }
  async function loadSnapshotIdentity(){
    try{const detail=await api('/catalog/34?preset=GENERAL_TQ'),trace=detail.tq_sell_trace||detail.tq_buy_trace;
      if(trace?.captureId)$('snapshotInfo').textContent=`${new Date(trace.capturedAt).toLocaleString()} · ${snapshotLabel(trace.aggregation)}`;
    }catch{/* The preview timestamp remains visible when identity lookup is unavailable. */}
  }
  function renderDashboard(){
    const s=state.preview?.summary;if(!s)return;
    $('dashboardCards').innerHTML=[metric('Items available to Buy & Sell',s.both_sides),metric('Need Attention',s.unresolved,true),metric('Items on market',s.seeded),metric('Items considered',s.marketable_types_considered)].join('');
  }
  function showPage(page){
    state.pageName=page;
    $('notice').classList.add('hidden');
    document.querySelectorAll('.page').forEach(el=>el.classList.toggle('hidden',el.id!==page+'Page'));
    document.querySelectorAll('.nav-item').forEach(el=>el.classList.toggle('active',el.dataset.page===page||el.dataset.page==='advanced'&&['rules','profiles'].includes(page)));
    $('expertPreviewActions').classList.toggle('hidden',!['advanced','rules','profiles'].includes(page));
    if(page==='catalog')loadCatalog();if(page==='dashboard')inspectQuick();
    if(page==='rules')renderRules();
    if(page==='profiles')renderProfiles();
    if(page==='advanced')loadBlueprintCatalog();
    if(page==='unresolved')renderUnresolved();
    if(page==='candidates')refreshCandidates();
  }
  async function loadTree(){
    try{const response=await api('/catalog/tree');state.tree=response.categories||[];
      const oldCat=$('categoryFilter').value;$('categoryFilter').innerHTML='<option value="">All categories</option>'+state.tree.map(c=>`<option value="${c.id}">${esc(c.label)} (${number(c.count)})</option>`).join('');$('categoryFilter').value=oldCat;
      renderGroupOptions();
    }catch(error){notice(`Catalog groups unavailable: ${error.message}`,true);}
  }
  function renderGroupOptions(){
    const old=$('groupFilter').value,category=$('categoryFilter').value;
    const groups=(state.tree||[]).filter(c=>!category||String(c.id)===category).flatMap(c=>c.groups||[]);
    const unique=new Map(groups.map(g=>[String(g.id),g]));
    $('groupFilter').innerHTML='<option value="">All groups</option>'+[...unique.values()].sort((a,b)=>a.name.localeCompare(b.name)).map(g=>`<option value="${g.id}">${esc(g.name)} (${number(g.count)})</option>`).join('');
    if(unique.has(old))$('groupFilter').value=old;
  }
  function filters(){
    const params=new URLSearchParams({limit:String(state.pageSize),offset:String(state.page*state.pageSize),preset:state.preset});
    const values={friendly_group:$('friendlyFilter').value,source:$('sourceFilter').value,q:$('catalogSearch').value.trim(),category:$('categoryFilter').value,group:$('groupFilter').value,
      fact:$('factFilter').value,profile:$('profileFilter').value,side:$('sideFilter').value};
    for(const [key,value] of Object.entries(values))if(value)params.set(key,value);
    if(params.get('friendly_group')?.startsWith('scope:')){params.set('scope_id',params.get('friendly_group').slice(6));params.delete('friendly_group');}
    const status=$('statusFilter').value;if(status==='unresolved')params.set('unresolved','true');else if(status==='seeded')params.set('seeded','true');else if(status==='unseeded')params.set('seeded','false');
    $('filterCount').textContent=Object.values(values).filter(Boolean).length+(status?1:0)||'';
    return params;
  }
  async function loadCatalog(){
    if(!state.policy)return;
    const serial=++state.catalogSerial;
    if(state.previewCurrent&&!filters().get('scope_id')){
      const f=filters(),q=(f.get('q')||'').toLowerCase(),source=f.get('source');
      const rows=state.preview.items.filter(item=>{
        const p=item.resolution?.policy,side=p?.sides||'unseeded';
        return (!q||item.name.toLowerCase().includes(q)||String(item.type_id).includes(q))&&
          (!f.get('friendly_group')||item.friendly_group===f.get('friendly_group'))&&
          (!f.get('category')||String(item.category_id)===f.get('category'))&&(!f.get('group')||String(item.group_id)===f.get('group'))&&
          (!f.get('fact')||item.facts?.includes(f.get('fact')))&&(!f.get('profile')||p?.profile_id===f.get('profile'))&&
          (!f.get('side')||side===f.get('side'))&&(!source||(source==='tq'?[p?.sell?.source,p?.buy?.source].some(x=>x?.startsWith('tq_')):[p?.sell?.source,p?.buy?.source].includes(source)))&&
          (!f.get('unresolved')||!!item.unresolved_reason)&&(!f.get('seeded')||(f.get('seeded')==='true')===(!!p&&!item.unresolved_reason));
      });
      state.catalogTotal=rows.length;state.catalog=rows.slice(state.page*state.pageSize,(state.page+1)*state.pageSize);renderCatalog();return;
    }
    $('catalogBody').innerHTML='<tr><td colspan="6" class="empty">Reading items…</td></tr>';
    try{const response=await api('/catalog?'+filters());if(serial!==state.catalogSerial)return;state.catalog=(response.items||[]).map(item=>state.previewCurrent?(state.previewMap.get(String(item.type_id))||item):item);state.catalogTotal=response.total??0;renderCatalog();}
    catch(error){notice(`Items unavailable: ${error.message}`,true);}
  }
  function reasonLabel(raw){const key=typeof raw==='object'?reason(raw):String(raw||'');return ({NO_TQ_REFERENCE:'No TQ price found',CROSSED_REFERENCE:'TQ Buy price is above Sell',ROUNDING_CROSS:'Equal after rounding',other:'Pricing needs review'})[key]||'Pricing needs review';}
  function catalogRow(item){
    const id=dataId(item),r=state.previewMap.get(String(id))||item,p=r.resolution?.policy,status=r.unresolved_reason?'Needs attention':p?'Ready':'Not on market';
    return `<tr class="clickable" data-type="${esc(id)}"><td><span class="item-name">${esc(item.name)}</span></td><td>${esc(item.category_name||'Other')}<span class="secondary">${esc(item.group_name||'—')}</span></td><td class="numeric price">${esc(price(r.buy_price))}</td><td class="numeric price">${esc(price(r.sell_price))}</td><td>${esc(availabilityLabel(p?.sides||item.side||'unseeded'))}</td><td><span class="badge ${r.unresolved_reason?'warn':p?'ok':''}">${esc(status)}</span></td></tr>`;
  }
  function renderCatalog(){
    $('catalogTotal').textContent=`${number(state.catalogTotal)} items`;
    $('catalogBody').innerHTML=state.catalog.map(catalogRow).join('')||'<tr><td colspan="6" class="empty">No items match these filters.</td></tr>';
    const first=state.page*state.pageSize+1,last=Math.min(state.catalogTotal,(state.page+1)*state.pageSize);
    $('catalogPageLabel').textContent=state.catalogTotal?`${number(first)}–${number(last)} of ${number(state.catalogTotal)}`:'No results';
    $('catalogPrev').disabled=state.page===0;$('catalogNext').disabled=last>=state.catalogTotal;
  }
  function reason(item){
    const raw=String(item.unresolved_reason||'').toUpperCase(),tq=String(item.tq?.state||'').toUpperCase();
    if(raw.includes('ROUNDING'))return 'ROUNDING_CROSS';
    if(tq==='CROSSED_REFERENCE'||raw.includes('CROSSED'))return 'CROSSED_REFERENCE';
    if(tq==='NO_TQ_REFERENCE'||raw.includes('REFERENCE'))return 'NO_TQ_REFERENCE';
    return 'other';
  }
  function nextStep(key){
    return key==='NO_TQ_REFERENCE'?'Leave it off the market or choose another supported price source':
      key==='CROSSED_REFERENCE'?'Review the conflicting Buy and Sell evidence':
      key==='ROUNDING_CROSS'?'Review the price percentages and rounding':'Open item details to review its pricing';
  }
  function renderUnresolved(){
    const all=(state.preview?.items||[]).filter(item=>item.unresolved_reason);
    const counts={NO_TQ_REFERENCE:0,CROSSED_REFERENCE:0,ROUNDING_CROSS:0,other:0};
    for(const item of all)counts[reason(item)]++;
    const names={NO_TQ_REFERENCE:'No TQ price found',CROSSED_REFERENCE:'TQ Buy above Sell',ROUNDING_CROSS:'Equal after rounding',other:'Other pricing issues'};
    $('reasonCards').innerHTML=Object.entries(counts).map(([key,value])=>`<button class="reason-card ${$('reasonFilter').value===key?'active':''}" data-reason="${key}"><strong>${number(value)}</strong><span>${names[key]}</span></button>`).join('');
    const shown=all.filter(item=>!$('reasonFilter').value||reason(item)===$('reasonFilter').value);
    $('reviewCount').textContent=`${number(shown.length)} items to review`;
    $('unresolvedBody').innerHTML=shown.map(item=>`<tr class="clickable" data-type="${esc(dataId(item))}"><td><span class="item-name">${esc(item.name)}</span></td><td>${esc(item.category_name||'Other')}<span class="secondary">${esc(item.group_name||'—')}</span></td><td><span class="badge warn">${esc(reasonLabel(item))}</span></td><td>${esc(item.tq?.average_price?'Average price available':'Open evidence')}</td><td>${esc(nextStep(reason(item)))}</td></tr>`).join('')||'<tr><td colspan="5" class="empty">No items in this review group.</td></tr>';
  }
  function selectorLabel(selector={}){
    if(selector.fact)return `Factual tag: ${title(selector.fact)}`;
    for(const [field,label] of [['type_ids','Exact type'],['group_ids','Group'],['category_ids','Category']])
      if(selector[field])return `${label}: ${selector[field].length===1?selector[field][0]:selector[field].length+' types'}`;
    return 'Unknown selector';
  }
  function renderRules(){
    if(!state.policy)return;
    const coverage=new Map((state.preview?.summary?.rule_coverage||[]).map(x=>[x.rule_id,x.match_count]));
    $('rulesBody').innerHTML=(state.policy.rules||[]).map((rule,index)=>`<tr><td><span class="item-name">${esc(title(rule.id))}</span><span class="item-id">${esc(rule.id)}</span></td><td>${esc(selectorLabel(rule.selector))}</td><td>${esc(rule.priority)}</td><td>${esc(rule.profile||'Inline settings')}</td><td>${coverage.has(rule.id)?number(coverage.get(rule.id)):'Preview to count'}</td><td><span class="badge ${rule.sides==='unseeded'?'warn':'ok'}">${rule.sides==='unseeded'?'Exclusion':'Active'}</span></td><td class="row-actions"><button data-rule-action="edit" data-index="${index}" ${builtIn()?'disabled':''}>Edit</button> <button data-rule-action="duplicate" data-index="${index}" ${builtIn()?'disabled':''}>Duplicate</button> <button data-rule-action="delete" data-index="${index}" ${builtIn()?'disabled':''}>Delete</button></td></tr>`).join('')||'<tr><td colspan="7" class="empty">No rules in this policy.</td></tr>';
  }
  function renderProfiles(){
    if(!state.policy)return;
    $('profileFilter').innerHTML='<option value="">All profiles</option>'+(state.policy.profiles||[]).map(p=>`<option value="${esc(p.id)}">${esc(title(p.id))}</option>`).join('');
    $('profilesGrid').innerHTML=(state.policy.profiles||[]).map((profile,index)=>{
      const used=state.policy.rules.filter(rule=>rule.profile===profile.id).length;
      return `<div class="profile-card"><div class="profile-top"><div><h2>${esc(title(profile.id))}</h2><span class="secondary">${esc(profile.id)}</span></div><span class="badge ok">${esc(title(profile.sides))}</span></div><div class="profile-prices"><div><strong>Sell</strong><span>${esc(labels[profile.sell?.source]||profile.sell?.source||'Disabled')}</span><small>${esc(number(Number(profile.sell?.multiplier)*100))}%</small></div><div><strong>Buy</strong><span>${esc(labels[profile.buy?.source]||profile.buy?.source||'Disabled')}</span><small>${esc(number(Number(profile.buy?.multiplier)*100))}%</small></div></div><p class="muted">Used by ${number(used)} rule${used===1?'':'s'}</p><div class="profile-actions"><button data-profile-action="edit" data-index="${index}" ${builtIn()?'disabled':''}>Edit / rename</button><button data-profile-action="duplicate" data-index="${index}" ${builtIn()?'disabled':''}>Duplicate</button></div></div>`;
    }).join('')||'<div class="card">No profiles in this policy.</div>';
  }
  function closeDrawer(){$('itemDrawer').classList.add('hidden');$('drawerBackdrop').classList.add('hidden');}
  async function selectType(id){
    if(!state.previewCurrent&&state.dirty)await previewData(false);state.selected=String(id);$('itemDrawer').classList.remove('hidden');$('drawerBackdrop').classList.remove('hidden');
    $('detailName').textContent='Loading item…';$('detailMeta').textContent=`Type ${id}`;$('detailBody').innerHTML='<p class="muted">Resolving price and evidence…</p>';
    try{const response=await api(`/catalog/${encodeURIComponent(id)}?preset=${encodeURIComponent(state.preset)}`);
      const preview=state.previewCurrent?state.previewMap.get(String(id)):null;
      state.detail={...response,preview:preview||response.preview,resolution:preview?.resolution||response.resolution};
      renderDetail();
    }catch(error){$('detailBody').innerHTML=`<div class="warning-box">${esc(error.message)}</div>`;notice(`Item details failed: ${error.message}`,true);}
  }
  function renderDetail(){
    const d=state.detail,p=d.preview||{},r=d.resolution||{},policy=r.policy||{},tq=p.tq||{},id=dataId(d);
    $('detailName').textContent=d.name||`Type ${id}`;
    $('detailMeta').textContent=`${p.category_name||d.category_name||state.tree?.find(c=>c.id===d.category_id)?.label||'Other'} · ${d.group_name||p.group_name||''}`;
    const side=policy.sides||'unseeded';
    const sourceText=value=>labels[value]||title(value||'Unavailable');
    const evidence=(tq.hubs||[]).map(h=>`<div class="evidence-row"><span>${esc(title(h.hub))}${tq.aggregation?.startsWith('jita_')&&h.hub!=='jita'?' (comparison only)':''}</span><span>Buy ${esc(price(h.buy?.bestPrice))} · Sell ${esc(price(h.sell?.bestPrice))}</span></div>`).join('');
    const authority=d.price_provenance||p.price_provenance||{};
    const authorityHtml=authority.authority==='sde_variation_parent_final_quote'?`<p>Linked T1 type: ${esc(authority.parent_type_id)}. Parent NPC Buy: ${esc(price(authority.reference_buy))}; parent NPC Sell: ${esc(price(authority.reference_sell))}. Your side multipliers apply to these final T1 quotes.</p>`:authority.authority==='npc_acquisition'?`<p>NPC acquisition reference: ${esc(price(authority.reference))}. Evidence: ${esc(authority.provenance)}.</p>`:'';
    const history=tq.history_blend||d.tq_buy_trace?.historyBlend||d.tq_sell_trace?.historyBlend;
    const historyHtml=history?`<h4>Trading history + snapshot</h4><p>The Forge executed trades · completed days before ${esc(history.asOfDate)}. Snapshot Buy/Sell are independently blended 50/50.</p><div class="evidence-row"><span>History window</span><strong>${history.windowDays?esc(history.windowDays)+' days · '+esc(history.tradingDays)+' trading days':'Unavailable · snapshot only'}</strong></div><div class="evidence-row"><span>Median daily average</span><strong>${esc(price(history.anchor))}</strong></div><div class="evidence-row"><span>Raw Jita snapshot</span><strong>Buy ${esc(price(history.snapshotBuy))} · Sell ${esc(price(history.snapshotSell))}</strong></div><div class="evidence-row"><span>Last trade / volume</span><strong>${esc(history.lastTradeDate||'—')} · ${esc(number(history.totalTradedVolume))}</strong></div>`:'';
    const quoteWarning=(p.warnings||[]).includes('BUY_AT_OR_ABOVE_SELL')?'<p class="hint">Buy >= Sell: prices retained as configured. Review warning; building is allowed.</p>':'';
    const warning=p.unresolved_reason||r.exclusion_reason;
    $('detailBody').innerHTML=`
      <div class="quote-grid"><div class="quote buy"><span>NPC BUY · FROM YOU</span><strong>${esc(price(p.buy_price))}</strong><small>${esc(sourceText(policy.buy?.source))}</small></div><div class="quote sell"><span>NPC SELL · TO YOU</span><strong>${esc(price(p.sell_price))}</strong><small>${esc(sourceText(policy.sell?.source))}</small></div></div>
      <div class="detail-summary"><div><small>Market</small><strong>${esc(availabilityLabel(side))}</strong></div><div><small>Status</small><strong>${esc(p.unresolved_reason?'Needs attention':policy.sides?'Ready':'Not on market')}</strong></div><div><small>Buy pricing</small><strong>${esc(sourceText(policy.buy?.source))}</strong></div><div><small>Sell pricing</small><strong>${esc(sourceText(policy.sell?.source))}</strong></div></div>
      ${(d.economic_family||p.economic_family)?.warnings?.length?`<div class="warning-box">${(d.economic_family||p.economic_family).warnings.map(esc).join("<br>")}</div>`:''}
      ${(d.blueprint||p.blueprint)?.warnings?.length?`<div class="warning-box"><strong>Blueprint semantics · warning only</strong><br>${(d.blueprint||p.blueprint).warnings.map(esc).join('<br>')}</div>`:''}
      ${(d.structure||p.structure)?.warnings?.length?`<div class="warning-box"><strong>Structure classification</strong><br>${(d.structure||p.structure).warnings.map(esc).join('<br>')}</div>`:''}
      ${warning?`<div class="warning-box"><strong>Needs review</strong><br>${esc(p.unresolved_reason?reasonLabel(p):'This item is left off the market')}<br>${esc(nextStep(reason(p)))}</div>`:''}
      ${tq.warnings?.length?`<div class="warning-box">${tq.warnings.map(esc).join('<br>')}</div>`:''}
      <details><summary>Price details</summary>${authorityHtml}<p>TQ snapshot: ${esc(tq.captured_at||d.tq_buy_trace?.capturedAt||'Unavailable')} · ${esc(snapshotLabel(tq.aggregation||d.tq_buy_trace?.aggregation))}</p><div class="evidence-row"><span>TQ Buy reference</span><strong>${esc(price(tq.buy_reference))}</strong></div><div class="evidence-row"><span>TQ Sell reference</span><strong>${esc(price(tq.sell_reference))}</strong></div><div class="evidence-row"><span>ESI average (evidence only)</span><strong>${esc(price(tq.average_price))}</strong></div>${historyHtml}${evidence||'<p>No hub observations captured for this type.</p>'}${quoteWarning}<p>Industry adjusted price is never used for a TQ trade quote.</p></details>
      <details><summary>Advanced policy settings</summary><div class="evidence-row"><span>Availability</span><strong>${esc(title(side))}</strong></div><div class="evidence-row"><span>Profile</span><strong>${esc(policy.profile_id||'Inline rule')}</strong></div><div class="evidence-row"><span>Sell</span><strong>${esc(sourceText(policy.sell?.source))} × ${esc(policy.sell?.multiplier_text||policy.sell?.multiplier||'—')}</strong></div><div class="evidence-row"><span>Buy</span><strong>${esc(sourceText(policy.buy?.source))} × ${esc(policy.buy?.multiplier_text||policy.buy?.multiplier||'—')}</strong></div><div class="evidence-row"><span>Quantity</span><strong>${esc(number(p.quantity))}</strong></div></details>
      <details><summary>Advanced details</summary><p>Winning rule: ${esc(r.winner_rule_id||'None')} · matched rules: ${esc((r.matched_rules||[]).length)}</p><pre class="technical">${esc(JSON.stringify({resolution:r,tq_buy_trace:d.tq_buy_trace,tq_sell_trace:d.tq_sell_trace,price_provenance:d.price_provenance,facts:p.facts},null,2))}</pre></details>`;
  }
  function allowedSources(){
    return state.preset==='GENERAL_TQ'?(state.schema?.general_tq_price_sources||[]):(state.schema?.sources||[]);
  }
  function setSide(side,value){
    $(side+'Source').value=allowedSources().includes(value?.source)?value.source:(allowedSources()[0]||'');
    $(side+'Multiplier').value=String(value?.multiplier_text??value?.multiplier??'1.00');
  }
  function updateEditorVisibility(){
    const mode=state.editor?.kind;
    $('availability').querySelector('option[value="unseeded"]').textContent=mode==='blueprint'?'None (not on market)':'Unseeded';
    $('selectorFields').classList.toggle('hidden',mode==='profile'||mode==='blueprint');
    $('profileNameField').classList.toggle('hidden',mode!=='profile');
    $('ruleName').required=mode!=='profile'&&mode!=='blueprint';
    $('selectorValue').required=mode!=='profile'&&mode!=='blueprint';
    $('profileName').required=mode==='profile';
    $('profileChoiceLabel').classList.toggle('hidden',mode==='profile'||mode==='blueprint');
    $('profilePickerLabel').classList.toggle('hidden',mode==='profile'||mode==='blueprint'||$('settingsMode').value!=='profile');
    $('sideSettings').classList.toggle('hidden',mode!=='profile'&&$('settingsMode').value==='profile'||$('availability').value==='unseeded');
    const sell=$('availability').value==='sell_only'||$('availability').value==='buy_sell';
    const buy=$('availability').value==='buy_only'||$('availability').value==='buy_sell';
    $('sellSource').closest('.side-editor').classList.toggle('hidden',!sell);
    $('buySource').closest('.side-editor').classList.toggle('hidden',!buy);
  }
  function openEditor(kind,index=null){
    if(!state.policy||state.busy||state.loadingPreset)return;if(builtIn())return notice('Choose Edit a Copy before changing the preset.');
    state.editor={kind,index};renderSources();
    $('editorEyebrow').textContent=kind==='profile'?'REUSABLE PROFILE':kind==='override'?'EXACT ITEM OVERRIDE':'POLICY RULE';
    $('editorTitle').textContent=kind==='profile'?(index==null?'Create profile':'Edit profile'):kind==='override'?'Edit item override':index==null?'Create rule':'Edit rule';
    $('editorIntro').textContent=kind==='override'?'This change applies to the selected item only. It stays in your draft until you save.':kind==='profile'?'Profiles are reusable templates. Renaming updates rules that reference the profile.':'Choose an item group, category, factual tag or exact type.';
    $('editorMatch').textContent='';
    $('settingsMode').value='inline';$('priority').value=String(Math.max(0,...state.policy.rules.map(r=>r.priority))+10);
    $('profileName').value='';$('ruleName').value='';$('selectorKind').value='type_ids';$('selectorValue').value='';
    $('availability').value='buy_sell';setSide('sell',null);setSide('buy',null);
    $('editorProfile').innerHTML=(state.policy.profiles||[]).map(p=>`<option value="${esc(p.id)}">${esc(title(p.id))}</option>`).join('');
    if(kind==='override'){
      const d=state.detail,p=d?.resolution?.policy||{},id=state.selected;
      const existing=state.policy.rules.findIndex(r=>r.id===`override_type_${id}`);
      if(existing>=0){state.editor.index=existing;fillRule(state.policy.rules[existing]);}
      else{$('ruleName').value=`Override ${d?.name||('type '+id)}`;$('selectorValue').value=id;$('availability').value=p.sides||'buy_sell';
        setSide('sell',p.sell);setSide('buy',p.buy);$('priority').value=String(Math.max(1000,...(d?.resolution?.matched_rules||[]).map(x=>Number(x.priority||0)+1)));}
      $('selectorKind').value='type_ids';
      $('selectorValue').value=id;
    }else if(kind==='rule'&&index!=null)fillRule(state.policy.rules[index]);
    else if(kind==='profile'){
      if(index!=null)fillProfile(state.policy.profiles[index]);
      else{$('profileName').value='new_profile';$('editorMatch').textContent='A new profile does not affect items until a rule uses it.';}
    }
    const coverage=state.preview?.summary?.rule_coverage||[];
    if(kind==='rule'&&index!=null){const match=coverage.find(x=>x.rule_id===state.policy.rules[index].id);$('editorMatch').textContent=match?`Matched ${number(match.match_count)} types in the latest backend preview.`:'Run preview to count matched types.';}
    if(kind==='override')$('editorMatch').textContent='Exact type selector · affects one catalog type before backend conflict checks.';
    updateEditorVisibility();$('editorDialog').showModal();
  }
  function fillRule(rule){
    if(!rule)return;
    $('ruleName').value=rule.id;$('priority').value=rule.priority??50;
    const selector=rule.selector||{},field=selector.fact?'fact':selector.type_ids?'type_ids':selector.group_ids?'group_ids':'category_ids';
    $('selectorKind').value=field;$('selectorValue').value=field==='fact'?selector.fact:(selector[field]||[]).join(',');
    $('selectorValueLabel').firstChild.textContent={type_ids:'Type ID(s)',group_ids:'Group ID(s)',category_ids:'Category ID(s)',fact:'Factual tag'}[field];
    if(rule.profile){$('settingsMode').value='profile';$('editorProfile').value=rule.profile;const p=state.policy.profiles.find(x=>x.id===rule.profile);$('availability').value=rule.sides||p?.sides||'buy_sell';setSide('sell',p?.sell);setSide('buy',p?.buy);}
    else{$('settingsMode').value='inline';$('availability').value=rule.sides||'unseeded';setSide('sell',rule.sell);setSide('buy',rule.buy);}
  }
  function fillProfile(profile){
    $('profileName').value=profile.id;$('availability').value=profile.sides||'buy_sell';setSide('sell',profile.sell);setSide('buy',profile.buy);
    const used=state.policy.rules.filter(r=>r.profile===profile.id).length;$('editorMatch').textContent=`Used by ${used} rule${used===1?'':'s'}.`;
  }
  function slug(value){const s=String(value).toLowerCase().replace(/[^a-z0-9]+/g,'_').replace(/^_+|_+$/g,'').slice(0,46)||'rule';return /^[a-z]/.test(s)?s:'rule_'+s;}
  function sideValue(side){return {source:$(side+'Source').value,multiplier:$(side+'Multiplier').value.trim()};}
  async function applyEditor(event){
    event.preventDefault();
    const kind=state.editor?.kind,index=state.editor?.index,availability=$('availability').value;
    if(!kind)return;
    if(kind==='blueprint'){
      const settings={sides:availability,priority:Number($('priority').value)};
      if(['sell_only','buy_sell'].includes(availability))settings.sell=sideValue('sell');
      if(['buy_only','buy_sell'].includes(availability))settings.buy=sideValue('buy');
      try{const r=await post('/blueprints/apply',{policy:state.policy,preset:state.preset,family:state.editor.family,settings});
        state.policy=r.policy;$('editorDialog').close();markDirty();await syncQuick();await previewData(false);await loadBlueprintCatalog();
        notice(`Family settings applied to ${number(r.matched)} published blueprints. Review instance warnings and unresolved prices before building.`);
      }catch(error){notice(`Blueprint settings were not applied: ${error.message}`,true);}
      return;
    }
    let target;
    if(kind==='profile'){
      const name=$('profileName').value.trim();
      if(!name||state.policy.profiles.some((p,i)=>i!==index&&p.id===name))return notice('Profile name is empty or already used.',true);
      target=index==null?{id:name}:state.policy.profiles[index];
      const old=target.id;target.id=name;target.sides=availability;
      if(availability==='sell_only'||availability==='buy_sell')target.sell=sideValue('sell');else delete target.sell;
      if(availability==='buy_only'||availability==='buy_sell')target.buy=sideValue('buy');else delete target.buy;
      if(index==null)state.policy.profiles.push(target);
      if(old!==name)for(const rule of state.policy.rules)if(rule.profile===old)rule.profile=name;
    }else{
      const field=$('selectorKind').value,raw=$('selectorValue').value.trim(),name=$('ruleName').value.trim();
      if(!name)return notice('Rule name is required.',true);
      const values=field==='fact'?raw:raw.split(',').map(v=>Number(v.trim()));
      if(field==='fact'?!state.schema?.facts?.includes(raw):!values.length||values.some(v=>!Number.isInteger(v)||v<0))return notice('Choose a valid selector value.',true);
      target=index==null?{}:state.policy.rules[index];
      const old=target.id;
      target.id=kind==='override'?`override_type_${state.selected}`:slug(name);
      if(state.policy.rules.some((r,i)=>i!==index&&r.id===target.id))return notice('Rule ID already exists. Rename this rule.',true);
      target.selector=field==='fact'?{fact:raw}:{[field]:values};target.priority=Number($('priority').value)||0;
      target.sides=availability;
      if(availability==='unseeded'){delete target.profile;delete target.sell;delete target.buy;}
      else if($('settingsMode').value==='profile'){target.profile=$('editorProfile').value;delete target.sell;delete target.buy;}
      else{delete target.profile;if(availability==='sell_only'||availability==='buy_sell')target.sell=sideValue('sell');else delete target.sell;
        if(availability==='buy_only'||availability==='buy_sell')target.buy=sideValue('buy');else delete target.buy;}
      if(index==null)state.policy.rules.push(target);
    }
    $('editorDialog').close();markDirty();await syncQuick();renderRules();renderProfiles();notice(`${kind==='profile'?'Profile':'Rule'} applied to the browser draft. Preview and compare before saving.`);
    previewData(false);
  }
  async function validate(){
    if(!state.policy)return null;
    try{return await post('/validate',{policy:state.policy,preset:state.preset});}
    catch(error){notice(`Validation request failed: ${error.message}`,true);return null;}
  }
  async function previewData(open=true){
    const revision=state.revision;
    const validation=await validate();
    if(!validation)return null;
    if(!validation.valid){
      const errors=(validation.errors||[]).map(x=>x.message||x).join('; ');
      notice(`Policy needs changes: ${errors||'validation failed'}`,true);
      if(open){$('validationMessage').textContent=errors||'Validation failed.';$('previewDialog').showModal();}
      return null;
    }
    try{const response=await post('/preview',{policy:state.policy,preset:state.preset});if(revision!==state.revision)return null;acceptPreview(response);
      renderDashboard();loadCatalog();renderRules();renderUnresolved();
      if(open){const s=response.summary||{};$('previewSummary').innerHTML=[
        metric('Seeded',s.seeded),metric('Unseeded / excluded',s.excluded),metric('Buy',s.buy),metric('Sell',s.sell),
        metric('Both',s.both_sides??s.both),metric('Unresolved',s.unresolved,true),metric('Warnings',s.warnings??0)
      ].join('');$('validationMessage').textContent=`Valid policy · ${(validation.warnings||[]).length} validation warnings · no SQLite written.`;$('previewDialog').showModal();}
      notice('Policy preview complete. No database was written.');return response;
    }catch(error){notice(`Preview failed: ${error.message}`,true);return null;}
  }
  function deltaText(delta){
    if(!delta)return '—';
    return `${Number(delta.absolute)>=0?'+':''}${price(delta.absolute)} (${Number(delta.percent)>=0?'+':''}${number(delta.percent)}%)`;
  }
  async function compare(){
    if(!state.distributionPreview?.valid)return notice('Preview Distribution before building.',true);
    const validation=await validate();if(!validation?.valid)return notice('Resolve validation errors before comparing.',true);
    try{const response=await post('/compare',{policy:state.policy,baseline:state.preset}),s=response.summary||{};
      $('compareTitle').textContent=`Review Changes · compared with ${presetLabel(state.preset)}`;renderSetupChanges();
      $('compareSummary').innerHTML=[metric('Added',s.added),metric('Removed',s.removed),metric('Side changes',s.side_changes),metric('Price changes',s.price_changes)].join('');
      const changes=[...(response.changes||[])].sort((a,b)=>Math.max(Math.abs(Number(b.sell_delta?.percent)||0),Math.abs(Number(b.buy_delta?.percent)||0))-Math.max(Math.abs(Number(a.sell_delta?.percent)||0),Math.abs(Number(a.buy_delta?.percent)||0)));
      $('compareBody').innerHTML=changes.slice(0,250).map(x=>`<tr class="clickable" data-type="${esc(x.type_id||x.item||'')}"><td>${esc(x.name||x.type_id||'Item')}</td><td>${esc(title(x.change||x.kind||'Changed'))}</td><td>${esc(deltaText(x.sell_delta))}</td><td>${esc(deltaText(x.buy_delta))}</td></tr>`).join('')||'<tr><td colspan="4" class="empty">No differences from the selected baseline.</td></tr>';
      $('previewDialog').close();$('compareDialog').showModal();notice('Comparison complete. Intentional differences are informational.');
    }catch(error){notice(`Comparison failed: ${error.message}`,true);}
  }
  async function openSaved(){
    try{const response=await api('/policies'),saved=(response.policies||[]).filter(x=>!x.preset);
      $('savedPolicySelect').innerHTML=saved.map(x=>`<option value="${esc(x.id)}">${esc(x.display_name||x.id)}</option>`).join('');
      if(!saved.length)return notice('No saved custom policies yet.');
      $('savedDialog').showModal();
    }catch(error){notice(`Saved policies unavailable: ${error.message}`,true);}
  }
  async function openSave(){
    if(builtIn())return notice('Choose Edit a Copy to create your own preset.');
    const validation=await validate();if(!validation?.valid)return notice('Fix the pricing errors before saving.',true);
    $('saveId').value=state.displayName;$('saveDescription').value=state.description;$('saveAuthor').value=state.author;$('saveOverwrite').checked=false;$('saveDialog').showModal();
  }
  async function savePolicy(event){
    event.preventDefault();if(builtIn())return;
    const name=$('saveId').value.trim(),id=state.isNew?state.loadedId:state.loadedId;
    try{const response=await post('/save',{id,display_name:name,description:$('saveDescription').value,author:$('saveAuthor').value,policy:state.policy,preset:state.preset,distribution:state.distribution,overwrite:$('saveOverwrite').checked});
      state.description=$('saveDescription').value;state.author=$('saveAuthor').value;state.distribution=response.distribution||state.distribution;state.loadedId=response.id;state.displayName=name;state.isNew=false;state.dirty=false;refreshChrome();renderQuick();$('saveDialog').close();showPage('candidates');notice(`Saved ${name}. Review resolved prices and Preview Distribution before building a separate database.`);
    }catch(error){notice(`Save failed: ${error.message}`,true);}
  }
  async function refreshCandidates(){
    try{const response=await api('/candidates');state.candidates=response.candidates||[];
      const latest=state.candidates[0];$('latestCandidate').textContent=latest?`${latest.id} · built`:'No market databases built yet';
      $('candidatesBody').innerHTML=state.candidates.map(candidate=>{
        const summary=candidate.summary||{},sell=summary.sell_rows??summary.sell,buy=summary.buy_rows??summary.buy;
        const dateToken=candidate.id.match(/20\d{6}/)?.[0];
        const created=candidate.created_at||(dateToken?`${dateToken.slice(0,4)}-${dateToken.slice(4,6)}-${dateToken.slice(6,8)} (ID)`:'Not recorded');
        const audit=state.audits.get(candidate.id);
        return `<tr><td><span class="item-name">${esc(candidate.id)}</span></td><td>${esc(candidate.policy_id||'—')}</td><td>${esc(created)}</td><td>${esc(number(sell))} / ${esc(number(buy))}</td><td><span class="badge ok">Built</span></td><td><span class="badge ${audit==='Passed'?'ok':audit==='Failed'?'error':''}">${esc(audit||'Not checked here')}</span></td><td><button data-candidate-action="view" data-id="${esc(candidate.id)}">View report</button> <button data-candidate-action="audit" data-id="${esc(candidate.id)}">Verify Market Database</button> <button data-candidate-action="install" data-id="${esc(candidate.id)}">Install into EveJS</button></td></tr>`;
      }).join('')||'<tr><td colspan="7" class="empty">No market databases built yet.</td></tr>';
    }catch(error){notice(`Could not list market databases: ${error.message}`,true);}
  }
  function reportHtml(candidate,audit=false){
    const s=candidate.summary||candidate.policy_preview||{},entries=[
      ['Policy',candidate.policy_id],['Preset',candidate.preset],['Database file',candidate.database_path],['Market SHA256',candidate.market_sha256],
      ['Policy SHA256',candidate.policy_sha256],['TQ dataset SHA256',candidate.tq_dataset_sha256],['Distribution mode',candidate.distribution?.mode],['Distribution seed',candidate.distribution?.seed],['Selected stations',candidate.distribution?.selected_stations],['Distribution plan SHA256',candidate.distribution?.plan_sha256],
      ['Sell rows',s.sell_rows??s.sell],['Buy rows',s.buy_rows??s.buy],['Both sides',s.both_sides]
    ];
    const auditFields=audit?Object.entries(candidate).filter(([key])=>!['summary','policy_preview'].includes(key)).map(([key,value])=>[key==='canonical_stations'?'Expected NPC station selection':title(key),typeof value==='object'?JSON.stringify(value):value]):[];
    return `<h2>${audit?'Market Database Verification':'Market Database Report'}</h2><div class="report-grid">${[...entries,...auditFields].filter(([,v])=>v!=null).map(([k,v])=>`<div><small>${esc(k)}</small><strong>${esc(number(v))}</strong></div>`).join('')}</div><details class="advanced"><summary>Technical report</summary><pre class="technical">${esc(JSON.stringify(candidate,null,2))}</pre></details>`;
  }
  let installation=null, installationSerial=0, installationBusy=false;
  function installationControls(){
    const ready=installation?.preview&&!installationBusy;
    $('installConfirm').textContent=installationBusy?'Installing…':installation?'Install database':'Installed';
    $('installConfirm').disabled=!ready||!$('installStopped').checked||(installation.preview.replaces_existing&&!$('installReplace').checked);
    $('installTarget').disabled=installationBusy;
    for(const el of $('installDialog').querySelectorAll('[data-close]'))el.disabled=installationBusy;
    $('installStopped').disabled=!ready;$('installReplace').disabled=!ready;
  }
  async function openInstallation(id){
    installation={id,preview:null};installationBusy=false;const openSerial=++installationSerial;
    $('installChecks').classList.remove('hidden');
    $('installError').classList.add('hidden');$('installSummary').textContent='Loading EveJS installations…';
    $('installStopped').checked=false;$('installReplace').checked=false;$('installReplaceLabel').classList.add('hidden');
    $('installTarget').innerHTML='';$('installDialog').showModal();installationControls();
    try{const response=await api('/installation/targets');
      if(!$('installDialog').open||installation?.id!==id||openSerial!==installationSerial)return;
      $('installTarget').innerHTML=response.targets.map(t=>`<option value="${esc(t.id)}" ${t.available?'':'disabled'}>${esc(t.label)}${t.available?'':': '+esc(t.error)}</option>`).join('');
      const available=response.targets.find(t=>t.available);if(!available)throw new Error('No usable EveJS destination. Check market-server.local.toml.');
      $('installTarget').value=available.id;await previewInstallation();
    }catch(error){if(openSerial!==installationSerial||!$('installDialog').open)return;$('installError').textContent=error.message;$('installError').classList.remove('hidden');}
  }
  async function previewInstallation(){
    if(!installation||installationBusy)return;
    const serial=++installationSerial;installation.preview=null;
    $('installError').classList.add('hidden');$('installStopped').checked=false;$('installReplace').checked=false;
    $('installReplaceLabel').classList.add('hidden');$('installSummary').textContent='Verifying the market database and checking the destination…';installationControls();
    try{const preview=await post(`/candidates/${encodeURIComponent(installation.id)}/install-preview`,{target_id:$('installTarget').value});
      if(serial!==installationSerial||!$('installDialog').open)return;
      installation.preview=preview;state.audits.set(installation.id,'Passed');
      $('installSummary').innerHTML=`<div><small>Market database</small><strong>${esc(preview.candidate_id)}</strong></div><div><small>EveJS folder</small><strong>${esc(preview.evejs_root)}</strong></div><div><small>Market-server database destination</small><strong>${esc(preview.database_path)}</strong></div><div><small>Size</small><strong>${number(preview.candidate_bytes)} bytes</strong></div><p>${preview.replaces_existing?'An existing database will be backed up in the same folder before replacement.':'No database exists at this path. Installation will create it.'}</p>${preview.wal_recovery_required?`<p class="warning-box">Leftover database journal found. Installation will back up the complete database and journal, then safely finalize them. Keep market-server stopped.</p>`:''}<p>After installation, use StartMarketServer.bat in this EveJS folder.</p>`;
      $('installReplaceLabel').classList.toggle('hidden',!preview.replaces_existing);installationControls();
    }catch(error){if(serial!==installationSerial)return;$('installSummary').textContent='Installation is blocked.';$('installError').textContent=error.message;$('installError').classList.remove('hidden');installationControls();}
  }
  async function installCandidate(){
    if(!installation?.preview||installationBusy||$('installConfirm').disabled)return;
    installationBusy=true;installationControls();$('installError').classList.add('hidden');
    try{const preview=installation.preview;
      const result=await post(`/candidates/${encodeURIComponent(installation.id)}/install`,{target_id:preview.target_id,preview_sha256:preview.preview_sha256,confirm_replace:$('installReplace').checked,server_stopped:$('installStopped').checked});
      $('installSummary').innerHTML=`<h3>Database installed</h3><p class="install-path">${esc(result.database_path)}</p>${result.backup_path?`<p>Previous database backup:</p><p class="install-path">${esc(result.backup_path)}</p>`:''}${result.raw_wal_backup_path?`<p>Database and journal recovery backup:</p><p class="install-path">${esc(result.raw_wal_backup_path)}</p>`:''}<p>${esc(result.next_step)}</p><p>No services were started or stopped.</p>${result.warning?`<p class="warning-box">${esc(result.warning)}</p>`:''}`;
      $('installChecks').classList.add('hidden');$('installSummary').closest('.dialog-body').scrollTop=0;
      installation=null;notice(`Installed database: ${result.database_path}. StartMarketServer will use it on its next start.`);
    }catch(error){$('installError').textContent=error.message;$('installError').classList.remove('hidden');}
    finally{installationBusy=false;installationControls();}
  }
  async function candidateAction(action,id){
    if(action==='install')return openInstallation(id);
    try{const response=action==='audit'?await post(`/candidates/${encodeURIComponent(id)}/audit`,{}):await api(`/candidates/${encodeURIComponent(id)}`);
      if(action==='audit'){state.audits.set(id,response.passed?'Passed':'Failed');await refreshCandidates();}
      $('candidateReport').innerHTML=reportHtml(response,action==='audit');$('candidateReport').classList.remove('hidden');
      $('candidateReport').scrollIntoView({block:'nearest'});notice(action==='audit'?`Market database verification complete for ${id}.`:`Opened report for ${id}.`);
    }catch(error){notice(`Market database ${action==='audit'?'verification':'report'} failed: ${error.message}`,true);}
  }
  async function buildCandidate(){
    if(state.dirty||state.loadedId===state.preset)return notice('Save a validated custom policy before building.',true);
    const validation=await validate();if(!validation?.valid)return notice('Resolve policy validation errors before building.',true);
    if(!window.confirm('Build a separate market database from this saved preset? No gameplay files will be changed.'))return;
    try{const response=await post('/candidates/build',{policy_id:state.loadedId,distribution_preview_sha256:state.distributionPreview.plan_sha256});notice(`Market database built: ${response.id||response.candidate_id||'complete'}.`);await refreshCandidates();}
    catch(error){notice(`Market database build failed: ${error.message}`,true);}
  }

  async function inspectQuick(){
    if(!state.policy)return;
    try{const r=await post('/quick-setup/inspect',{policy:state.policy,preset:state.preset});state.quick=r;renderQuick();}
    catch(error){notice(`Quick Setup could not load: ${error.message}`,true);}
  }
  async function syncQuick(){
    try{const r=await post('/quick-setup/sync',{policy:state.policy,preset:state.preset});state.policy=r.policy;state.quick=r;renderQuick();}
    catch(error){await inspectQuick();notice(`Quick Setup synchronization: ${error.message}`,true);}
  }
  function quickControls(scope){
    const s=scope.settings,disabled=builtIn()||state.busy;
    const sources=[['tq','TQ Market Prices'],['funded_cost','Production Cost'],['preset','Keep preset pricing']];
    const sideSources=[['','Keep group pricing'],...(state.preset==='GENERAL_TQ'?(state.schema?.general_tq_price_sources||[]):(state.schema?.sources||[])).map(x=>[x,labels[x]||title(x)])];
    const sideControl=(side,label)=>`<label>${label}<select name="${side}_source">${sideSources.map(([v,l])=>`<option value="${esc(v)}" ${(s[side+'_source']||'')===v?'selected':''}>${esc(l)}</option>`).join('')}</select></label>`;
    const sourceControls=scope.explicit_advanced?sideControl('buy','Buy price source')+sideControl('sell','Sell price source'):`<label>Price source<select name="source">${sources.map(([v,l])=>`<option value="${v}" ${s.source===v?'selected':''}>${l}</option>`).join('')}</select></label>`;
    const availability=[['buy_sell','Buy + Sell'],['sell_only','Sell only'],['buy_only','Buy only'],['unseeded','Not on market']];
    if(state.preset==='LEGACY_V1')availability.unshift(['preset','Keep preset availability']);
    return `<form class="quick-form" data-scope="${esc(scope.id)}"><fieldset ${disabled?'disabled':''}><div class="quick-fields"><label>Market<select name="availability">${availability.map(([v,l])=>`<option value="${v}" ${s.availability===v?'selected':''}>${l}</option>`).join('')}</select></label>${sourceControls}<label>NPC Buy price (%)<input name="buy_percent" type="number" value="${esc(s.buy_percent)}" step="any" min="0.0001" required></label><label>NPC Sell price (%)<input name="sell_percent" type="number" value="${esc(s.sell_percent)}" step="any" min="0.0001" required></label></div><div class="quick-actions"><button type="submit">Apply settings</button>${scope.custom?`<button type="button" data-reset-scope="${esc(scope.id)}">${scope.parent?'Reset to parent':'Reset to preset'}</button>`:''}<span class="muted">${scope.parent?'A custom subgroup overrides this group.':'Other settings and expert overrides are preserved.'}</span></div></fieldset></form>`;
  }
  function quickOverlapNote(scope){
    const names=scope.higher_priority_overlaps||[];
    return names.length?`<p class="muted subgroup-priority-note">When customized, ${names.map(esc).join(', ')} override this subgroup for matching items.</p>`:'';
  }
  function renderQuick(){
    if(!state.quick)return;
    const scopes=state.quick.state.scopes,byId=new Map(scopes.map(s=>[s.id,s]));
    const selectedFilter=$('friendlyFilter').value;
    $('friendlyFilter').innerHTML='<option value="">All market groups</option>'+state.quick.mapping.groups.map(g=>`<option value="${esc(g.id)}">${esc(g.label)}</option>`).join('')+'<optgroup label="Structures Advanced">'+scopes.filter(s=>s.structure_kind).map(s=>`<option value="scope:${esc(s.id)}">${esc(s.label)}</option>`).join('')+'</optgroup>'+['resources','industry','other'].map(id=>`<optgroup label="${esc(state.quick.mapping.groups.find(g=>g.id===id).label)} Advanced">${scopes.filter(s=>s.parent===id&&s.family_section).map(s=>`<option value="scope:${esc(s.id)}">${esc(s.label)}</option>`).join('')}</optgroup>`).join('');
    if([...$('friendlyFilter').options].some(o=>o.value===selectedFilter))$('friendlyFilter').value=selectedFilter;
    $('quickGroups').innerHTML=state.quick.mapping.groups.map(group=>{
      const scope=byId.get(group.id),children=scopes.filter(c=>c.parent===group.id&&c.count>0&&!c.hidden);
      const published=children.filter(c=>c.published_blueprint_family);
      const familyNote=published.length?`<p class="muted">${number(published.reduce((sum,c)=>sum+c.count,0))} published blueprints. Additional types enter the policy only when you apply family settings. ${esc(published[0].warning)} Missing prices remain unresolved.</p>`:'';
      const childHtml=child=>`<section class="subgroup" data-child="${esc(child.id)}"><div class="subgroup-heading"><strong>${esc(child.label)}</strong><span class="badge ${child.custom?'warn':''}">${child.custom?'Custom':'Inherit from '+esc(byId.get(child.inherits_from)?.label||group.label)}</span><span class="muted">${number(child.count)} items${child.no_market_group?` · ${number(child.no_market_group)} published opt-in`:''}</span>${(child.structure_kind||child.family_section)?`<button type="button" data-browse-scope="${esc(child.id)}">View items</button>`:''}${!child.custom?`<button type="button" data-customize-scope="${esc(child.id)}" ${builtIn()?'disabled':''}>Customize</button>`:''}</div>${child.structure_kind==='cross_section'?'<p class="muted">Fuel Blocks stay in Other Market Items. Apply this filter explicitly to override them here. Strontium is not included.</p>':''}${child.family_opt_in?'<p class="muted">Explicit opt-in: these published types are not added by a broad or default setting.</p>':''}${child.warning&&child.family_section?`<p class="warning-box">${esc(child.warning)}</p>`:''}${quickOverlapNote(child)}<div class="subgroup-controls ${child.custom?'':'hidden'}">${quickControls(child)}</div></section>`;
      const structureGroups=[['Main families',['primary']],['Overlapping filters',['technology','cross_section','legacy_overlay']],['POS functions',['pos_function']],['Additional published types',['additional_published']],['Previously saved controls',[null]]];
      const structureNote=group.id==='structures'?`<p class="muted">Simple keeps its ${number(scope.count)} original types. Apply a subgroup to include published types without a market group. Installed or conversion objects may have special gameplay semantics; missing prices stay unresolved.</p>`:'';
      const childrenHtml=group.id==='structures'?structureGroups.map(([label,kinds])=>{const selected=children.filter(c=>kinds.includes(c.structure_kind));return selected.length?`<h3>${label}</h3>${selected.map(childHtml).join('')}`:'';}).join(''):['resources','industry','other'].includes(group.id)?[...new Set(children.map(c=>c.family_section||'Aggregate controls'))].map(section=>`<h3>${esc(section)}</h3>${children.filter(c=>(c.family_section||'Aggregate controls')===section).map(childHtml).join('')}`).join(''):children.map(childHtml).join('');
      return `<article class="quick-card" data-group="${esc(group.id)}"><header><div><h2>${esc(group.label)}</h2><p>${esc(group.description)}</p></div><span class="badge">${number(scope.count)} items</span></header>${scope.contains_advanced_overrides?`<div class="override-note">Contains advanced overrides${scope.expert_count?` · ${number(scope.expert_count)} items protected`:''}</div>`:''}${quickControls(scope)}<details class="quick-subgroups" data-scope-open="${esc(group.id)}" ${state.openScopes.has(group.id)?'open':''}><summary>Advanced <span>${scope.custom_children?`${scope.custom_children} custom subgroup overrides`:'Subgroups inherit by default'}</span></summary>${familyNote}${structureNote}${childrenHtml||'<p class="muted">Use Items for individual changes or Advanced for other groupings.</p>'}</details></article>`;
    }).join('');
    $('quickGroups').querySelectorAll('[data-scope-open]').forEach(d=>d.addEventListener('toggle',()=>{if(d.open)state.openScopes.add(d.dataset.scopeOpen);else state.openScopes.delete(d.dataset.scopeOpen);}));
  }
  async function applyQuick(scope,settings){
    if(builtIn()||state.busy)return;
    state.busy=true;refreshChrome();renderQuick();notice('Applying settings to your draft…');
    try{const r=await post('/quick-setup/apply',{policy:state.policy,preset:state.preset,scope_id:scope,settings});state.policy=r.policy;state.quick=r;markDirty();await previewData(false);state.busy=false;refreshChrome();renderQuick();notice('Settings applied. Review Changes shows the affected items and prices.');}
    catch(error){state.busy=false;refreshChrome();renderQuick();notice(`Settings were not applied: ${error.message}`,true);}
  }
  function renderSetupChanges(){
    const q=state.quick?.state,scopes=new Map((q?.scopes||[]).map(x=>[x.id,x]));
    const groups=Object.entries(q?.changes||{}).map(([id,s])=>{const scope=scopes.get(id);return `<div class="setup-change"><strong>${esc(scope?.label||id)}</strong><span>${scope?.parent?'Subgroup':'Broad group'} · ${number(scope?.count)} matched items</span><span>${esc(availabilityLabel(s.availability))} · Buy ${esc(s.buy_percent)}% / Sell ${esc(s.sell_percent)}%</span></div>`;});
    const exact=state.policy.rules.filter(r=>r.id.startsWith('override_type_')).map(r=>`<div class="setup-change"><strong>${esc(state.previewMap.get(String(r.selector.type_ids?.[0]))?.name||'Exact item override')}</strong><span>Exact item · ${esc(availabilityLabel(r.sides))}</span></div>`);
    const other=(q?.expert_rule_ids||[]).filter(id=>!id.startsWith('override_type_'));
    $('setupChanges').innerHTML=`<h3>Distribution</h3><p>${esc(distributionDescriptions[state.distribution?.mode]||'5 Trade Hubs')} · seed ${esc(state.distribution?.seed||'12345')}. Preview Distribution shows exact station effects.</p>`+'<h3>Your item setup changes</h3>'+[...groups,...exact].join('')+(other.length?`<p>${other.length} additional Advanced overrides are preserved.</p>`:'')+(!groups.length&&!exact.length&&!other.length?'<p>No setup changes from the built-in preset.</p>':'');
  }
  async function reviewChanges(){
    if(await previewData(false)){await inspectQuick();await compare();}
  }
  function openItemEditor(){
    if(builtIn())return notice('Choose Edit a Copy to customize an item.');
    const p=state.detail?.resolution?.policy;$('itemEditTitle').textContent=state.detail?.name||'Edit this item';
    $('itemAvailability').value=p?.sides||'unseeded';$('itemSource').value=state.preset==='GENERAL_TQ'?'tq':'preset';
    $('itemBuyPercent').value='100';$('itemSellPercent').value='100';
    const r=state.policy.rules.find(r=>r.id===`override_type_${state.selected}`),desc=r?.description;
    if(desc?.startsWith('Item settings: ')){try{const set=JSON.parse(desc.slice(15));$('itemAvailability').value=set.availability;$('itemSource').value=set.source;$('itemBuyPercent').value=set.buy_percent;$('itemSellPercent').value=set.sell_percent;}catch{}}
    $('itemEditDialog').showModal();
  }
  function bindSimple(){
    $('welcomePanel').classList.toggle('hidden',localStorage.getItem('workbench-welcome-dismissed')==='1');
    $('dismissWelcome').onclick=()=>{localStorage.setItem('workbench-welcome-dismissed','1');$('welcomePanel').classList.add('hidden');};
    $('welcomeTq').onclick=()=>loadPreset('GENERAL_TQ');$('welcomeLegacy').onclick=()=>loadPreset('LEGACY_V1');$('welcomeSaved').onclick=openSaved;
    $('copyForm').onsubmit=event=>{event.preventDefault();const name=$('copyName').value.trim();if(!name)return;state.displayName=name;state.loadedId=slug(name)+'-'+Date.now();state.isNew=true;markDirty();$('copyDialog').close();showPage('dashboard');renderQuick();notice('Your copy is ready. Choose market groups and adjust prices below.');};
    $('quickGroups').addEventListener('submit',event=>{const form=event.target.closest('.quick-form');if(!form)return;event.preventDefault();const scope=state.quick?.state?.scopes.find(x=>x.id===form.dataset.scope);const settings={source:scope?.settings.source||'preset',...Object.fromEntries(new FormData(form))};for(const key of ['buy_source','sell_source'])if(!settings[key])delete settings[key];applyQuick(form.dataset.scope,settings);});
    $('quickGroups').addEventListener('click',event=>{const browse=event.target.closest('[data-browse-scope]'),reset=event.target.closest('[data-reset-scope]'),customize=event.target.closest('[data-customize-scope]');if(browse){$('friendlyFilter').value='scope:'+browse.dataset.browseScope;$('catalogSearch').value='';for(const id of ['categoryFilter','groupFilter','factFilter','profileFilter','sideFilter','sourceFilter','statusFilter'])$(id).value='';renderGroupOptions();state.page=0;showPage('catalog');}if(reset)applyQuick(reset.dataset.resetScope,null);if(customize){const section=customize.closest('.subgroup');section.querySelector('.subgroup-controls').classList.remove('hidden');customize.classList.add('hidden');}});
    $('reviewWorkflow').onclick=reviewChanges;$('saveWorkflow').onclick=openSave;
    $('resolutionDetailsBtn').onclick=()=>previewData(true);
    $('validateWorkflow').onclick=async()=>{const r=await validate();notice(r?.valid?'Preset is valid. Review and save it before building.':`Preset needs attention: ${(r?.errors||[]).join('; ')}`,!r?.valid);};
    $('advancedItemEdit').onclick=()=>{$('itemEditDialog').close();openEditor('override');};
    $('itemEditForm').onsubmit=async event=>{event.preventDefault();if(builtIn())return;
      try{const r=await post('/quick-setup/item',{policy:state.policy,preset:state.preset,type_id:Number(state.selected),settings:{availability:$('itemAvailability').value,source:$('itemSource').value,buy_percent:$('itemBuyPercent').value,sell_percent:$('itemSellPercent').value}});
        state.policy=r.policy;state.quick=r;markDirty();renderQuick();$('itemEditDialog').close();await previewData(false);await selectType(state.selected);notice('Item settings applied. Review Changes lists this exact item override.');}
      catch(error){notice(`Item settings were not applied: ${error.message}`,true);}
    };
  }

  function bind(){
    document.querySelectorAll('[data-page],[data-go]').forEach(button=>button.addEventListener('click',()=>showPage(button.dataset.page||button.dataset.go)));
    document.querySelectorAll('[data-close]').forEach(button=>button.addEventListener('click',()=>$(button.dataset.close).close()));
    $('preset').addEventListener('change',()=>{
      const wanted=$('preset').value;
      if(state.dirty&&!window.confirm('Discard unsaved changes and load another preset?')){$('preset').value=state.preset;return;}
      loadPreset(wanted);
    });
    $('loadPresetBtn').onclick=()=>loadPreset(state.isNew?state.preset:state.loadedId||state.preset);
    $('loadSavedBtn').onclick=openSaved;
    $('confirmLoadSavedBtn').onclick=()=>loadSaved($('savedPolicySelect').value);
    $('duplicateBtn').onclick=()=>{if(state.loadingPreset)return;$('copyName').value='My Market';$('copyDialog').showModal();};
    $('previewBtn').onclick=reviewChanges;
    $('dashboardPreview').onclick=reviewChanges;
    $('compareFromPreview').onclick=compare;
    $('saveBtn').onclick=openSave;
    $('saveForm').addEventListener('submit',savePolicy);
    $('catalogSearchBtn').onclick=()=>{state.page=0;loadCatalog();};
    $('catalogSearch').addEventListener('keydown',event=>{if(event.key==='Enter'){state.page=0;loadCatalog();}});
    $('clearSearch').onclick=()=>{$('catalogSearch').value='';state.page=0;loadCatalog();};
    $('filterToggle').onclick=()=>{const expanded=$('filterPanel').classList.toggle('hidden')===false;$('filterToggle').setAttribute('aria-expanded',String(expanded));};
    for(const id of ['categoryFilter','groupFilter','factFilter','profileFilter','sideFilter','statusFilter','friendlyFilter','sourceFilter']){
      $(id).addEventListener('change',()=>{if(id==='categoryFilter')renderGroupOptions();state.page=0;loadCatalog();});
    }
    $('catalogPrev').onclick=()=>{state.page=Math.max(0,state.page-1);loadCatalog();};
    $('catalogNext').onclick=()=>{if((state.page+1)*state.pageSize<state.catalogTotal){state.page++;loadCatalog();}};
    $('catalogBody').addEventListener('click',event=>{const row=event.target.closest('[data-type]');if(row)selectType(row.dataset.type);});
    $('unresolvedBody').addEventListener('click',event=>{const row=event.target.closest('[data-type]');if(row)selectType(row.dataset.type);});
    $('reasonCards').addEventListener('click',event=>{const card=event.target.closest('[data-reason]');if(card){$('reasonFilter').value=card.dataset.reason;renderUnresolved();}});
    $('reasonFilter').onchange=renderUnresolved;
    $('closeDrawer').onclick=closeDrawer;$('drawerBackdrop').onclick=closeDrawer;
    $('editOverrideBtn').onclick=()=>{if(state.detail?.blueprint&&state.detail.market_group_id==null)openEditor('override');else openItemEditor();};
    $('createRuleBtn').onclick=()=>openEditor('rule');
    $('createProfileBtn').onclick=()=>openEditor('profile');
    $('rulesBody').addEventListener('click',event=>{
      const button=event.target.closest('[data-rule-action]');if(!button)return;
      const index=Number(button.dataset.index),rule=state.policy.rules[index];
      if(button.dataset.ruleAction==='edit')openEditor('rule',index);
      else if(button.dataset.ruleAction==='duplicate'){
        openEditor('rule',null);fillRule(rule);$('ruleName').value=rule.id+' copy';$('priority').value=String(Number(rule.priority||0)+1);
        $('editorTitle').textContent='Duplicate rule';$('editorMatch').textContent='This copy will be added to the draft after you apply it.';updateEditorVisibility();
      }else if(button.dataset.ruleAction==='delete'){
        if(!window.confirm(`Delete rule ${rule.id} from the browser draft?`))return;
        state.policy.rules.splice(index,1);markDirty();renderRules();notice(`Rule ${rule.id} removed from the draft.`);previewData(false);
      }
    });
    $('profilesGrid').addEventListener('click',event=>{
      const button=event.target.closest('[data-profile-action]');if(!button)return;
      const index=Number(button.dataset.index),profile=state.policy.profiles[index];
      if(button.dataset.profileAction==='edit')openEditor('profile',index);
      else{openEditor('profile');fillProfile(profile);$('profileName').value=profile.id+'_copy';$('editorTitle').textContent='Duplicate profile';}
    });
    $('editorForm').addEventListener('submit',applyEditor);
    $('settingsMode').onchange=updateEditorVisibility;
    $('availability').onchange=updateEditorVisibility;
    $('selectorKind').onchange=()=>{$('selectorValueLabel').firstChild.textContent={type_ids:'Type ID(s)',group_ids:'Group ID(s)',category_ids:'Category ID(s)',fact:'Factual tag'}[$('selectorKind').value];};
    $('installTarget').onchange=previewInstallation;
    $('installStopped').onchange=installationControls;$('installReplace').onchange=installationControls;
    $('installConfirm').onclick=installCandidate;
    $('installDialog').addEventListener('cancel',event=>{if(installationBusy)event.preventDefault();});
    $('refreshCandidatesBtn').onclick=refreshCandidates;
    $('buildCandidateBtn').onclick=buildCandidate;
    $('candidatesBody').addEventListener('click',event=>{const button=event.target.closest('[data-candidate-action]');if(button)candidateAction(button.dataset.candidateAction,button.dataset.id);});
    $('compareBody').addEventListener('click',event=>{const row=event.target.closest('[data-type]');if(row?.dataset.type){$('compareDialog').close();selectType(row.dataset.type);}});
    document.addEventListener('keydown',event=>{if(event.key==='Escape')closeDrawer();});
  }

  const distributionDescriptions={jita_only:'One full market in Jita.',five_hubs:'Full market in Jita, Amarr, Dodixie, Rens and Hek.',hubs_regional:'Five full trade hubs plus smaller NPC markets across regions.',all_npc:'Every eligible NPC station. Secondary stations receive different assortments, stock and local prices.',custom:'Configure NPC station selection and distribution manually.'};
  function refreshDistributionControls(){
    const readOnly=builtIn()||state.loadingPreset||state.busy||!state.distribution;
    $('distributionForm').querySelectorAll('input,select,button').forEach(el=>{el.disabled=readOnly;});
    $('distributionPreviewBtn').disabled=state.loadingPreset||state.busy||!state.policy||!state.distribution;
    $('distributionCopy').classList.toggle('hidden',!builtIn());$('distributionCopy').disabled=state.loadingPreset||state.busy;
    $('distributionReadOnly').classList.toggle('hidden',!builtIn());
    $('candidateDistributionPreview').disabled=state.loadingPreset||state.busy;
    if(state.distributionSchema&&!state.distributionSchema.topology_available){$('distRemoteness').disabled=true;}
  }
  async function loadDistributionSchema(){
    try{state.distributionSchema=await api('/distribution/schema');renderDistribution();}
    catch(error){notice(`Distribution unavailable: ${error.message}`,true);}
  }
  function renderDistribution(){
    const d=state.distribution;if(!d)return;
    $('distributionMode').value=d.mode;$('distributionDescription').textContent=distributionDescriptions[d.mode];
    const hubNames={60003760:'Jita',60008494:'Amarr',60011866:'Dodixie',60004588:'Rens',60005686:'Hek'};
    const hubs=state.distributionSchema?.canonical_hubs||[];
    const included=d.mode==='jita_only'?['Jita']:d.mode==='custom'?hubs.filter(h=>d.custom.canonical_hubs.includes(h.station_id)).map(h=>hubNames[h.station_id]):Object.values(hubNames);
    $('distributionHubs').innerHTML=`<strong>MAIN TRADE HUBS · ${esc(included.join(' · ')||'None selected')}</strong><span>Full resolved assortment · Maximum stock (2,147,483,647) · Base prices (100%)</span><small> Unresolved items stay off the market. Canonical hub settings are fixed.</small>`;
    const secondary=['hubs_regional','all_npc','custom'].includes(d.mode);
    $('distributionSecondary').classList.toggle('hidden',!secondary);
    $('distributionCustom').classList.toggle('hidden',d.mode!=='custom');
    $('distRegionalSelection').classList.toggle('hidden',!(d.mode==='hubs_regional'||d.mode==='custom'&&d.custom.selection==='regional'));
    $('distStationPicker').classList.toggle('hidden',d.custom.selection!=='explicit');
    const values={distAssortmentMin:d.secondary.assortment.min,distAssortmentMax:d.secondary.assortment.max,distStockMin:d.secondary.stock.min,distStockMax:d.secondary.stock.max,
      distBuyMin:d.secondary.buy_modifier.min,distBuyMax:d.secondary.buy_modifier.max,distSellMin:d.secondary.sell_modifier.min,distSellMax:d.secondary.sell_modifier.max,
      distRegionMin:d.regional.min,distRegionMax:d.regional.max,distSeed:d.seed,distRemoteness:d.secondary.remoteness,distCustomSelection:d.custom.selection};
    for(const [id,value] of Object.entries(values))$(id).value=value;
    $('distCustomHubs').innerHTML=hubs.map(h=>`<label><input type="checkbox" data-dist-hub="${h.station_id}" ${d.custom.canonical_hubs.includes(h.station_id)?'checked':''}>${esc(hubNames[h.station_id])}</label>`).join('');
    $('distCustomRegions').innerHTML=(state.distributionSchema?.regions||[]).map(r=>`<option value="${r.id}" ${d.custom.region_ids.includes(r.id)?'selected':''}>${esc(r.name)}</option>`).join('');
    const old=$('distOverrideScope').value;
    $('distOverrideScope').innerHTML=(state.distributionSchema?.scopes||[]).map(scope=>`<option value="${esc(scope.id)}">${scope.parent?'↳ ':''}${esc(scope.label)}</option>`).join('');
    if(old)$('distOverrideScope').value=old;
    $('distTopologyInfo').textContent=state.distributionSchema?.topology_available?'Uses actual stargate jumps; values stay within your ranges.':'Stargate topology unavailable. Remoteness stays Off.';
    renderDistributionOverride();renderChosenStations();renderDistributionPreview();refreshDistributionControls();
  }
  function readDistributionForm(){
    if(!state.distribution)return;
    const d=structuredClone(state.distribution),numeric=id=>Number($(id).value);
    d.mode=$('distributionMode').value;d.seed=$('distSeed').value;
    d.secondary.assortment={min:numeric('distAssortmentMin'),max:numeric('distAssortmentMax')};d.secondary.stock={min:numeric('distStockMin'),max:numeric('distStockMax')};
    d.secondary.buy_modifier={min:numeric('distBuyMin'),max:numeric('distBuyMax')};d.secondary.sell_modifier={min:numeric('distSellMin'),max:numeric('distSellMax')};d.secondary.remoteness=$('distRemoteness').value;
    d.regional={min:numeric('distRegionMin'),max:numeric('distRegionMax')};d.custom.selection=$('distCustomSelection').value;
    d.custom.region_ids=[...$('distCustomRegions').selectedOptions].map(el=>Number(el.value));
    d.custom.canonical_hubs=[...$('distCustomHubs').querySelectorAll('input:checked')].map(el=>Number(el.dataset.distHub));
    state.distribution=d;
  }
  function renderDistributionOverride(){
    const d=state.distribution;if(!d)return;
    const id=$('distOverrideScope').value,scope=state.distributionSchema?.scopes.find(s=>s.id===id),own=d.overrides[id]||{},parent=d.overrides[scope?.parent]||{};
    const assortment=own.assortment||parent.assortment||d.secondary.assortment,stock=own.stock||parent.stock||d.secondary.stock;
    $('distOverrideAssortment').checked=!!own.assortment;$('distOverrideStock').checked=!!own.stock;
    $('distOverrideAssortmentMin').value=assortment.min;$('distOverrideAssortmentMax').value=assortment.max;$('distOverrideStockMin').value=stock.min;$('distOverrideStockMax').value=stock.max;
    const parentName=state.distributionSchema?.scopes.find(s=>s.id===scope?.parent)?.label||'Secondary default';
    $('distOverrideInherited').textContent=`Inherits from ${parentName}. Reset removes only this Distribution override.`;
    $('distOverrideList').innerHTML=Object.entries(d.overrides).map(([key,o])=>`<div class="override-chip"><strong>${esc(state.distributionSchema?.scopes.find(s=>s.id===key)?.label||key)}</strong><span>${o.assortment?`Assortment ${o.assortment.min}–${o.assortment.max}% `:''}${o.stock?`Stock ${o.stock.min}–${o.stock.max}`:''}</span></div>`).join('')||'<p class="muted">All groups inherit.</p>';
  }
  function renderChosenStations(){
    const ids=state.distribution?.custom.station_ids||[];
    $('distChosenStations').innerHTML=ids.map(id=>`<label>${esc(state.stationNames.get(id)||'Saved NPC station')}<button type="button" data-dist-remove="${id}">Remove</button></label>`).join('')||'<p class="muted">No individual secondary stations selected.</p>';
  }
  async function findDistributionStations(){
    try{const r=await api('/distribution/stations?q='+encodeURIComponent($('distStationSearch').value)+'&limit=50');
      for(const s of r.stations)state.stationNames.set(s.station_id,s.name);
      $('distStationResults').innerHTML=r.stations.map(s=>`<label><input type="checkbox" data-dist-station="${s.station_id}" ${state.distribution.custom.station_ids.includes(s.station_id)?'checked':''}><span>${esc(s.name)}<small>${esc(s.region_name)} · ${s.nearest_hub_jumps??'No connected'} jumps</small></span></label>`).join('')||'<p>No eligible NPC stations found.</p>';refreshDistributionControls();
    }catch(error){notice(`Station search failed: ${error.message}`,true);}
  }
  function renderDistributionPreview(){
    const p=state.distributionPreview;
    $('distributionStatus').textContent=p?`${p.valid?'Ready':'Blocked'} · ${number(p.total_seed_rows)} predicted rows · seed ${state.distribution.seed}`:'Preview Distribution before building your market database.';
    $('distributionPreview').classList.toggle('hidden',!p);if(!p)return;
    const stationTable=rows=>`<div class="distribution-book-table table-scroll"><table><thead><tr><th>NPC station</th><th>Tier / region</th><th>Assortment</th><th>Types</th><th>Sell / Buy rows</th><th>Stock min–max</th><th>Buy / Sell</th><th>Jumps</th></tr></thead><tbody>${rows.map(s=>`<tr><td>${esc(s.name)}</td><td>${esc(title(s.tier))}<small>${esc(s.region)}</small></td><td>${number(s.actual_assortment_percent)}%<small>target ${number(s.assortment_target_percent)}%</small></td><td>${number(s.types)}</td><td>${number(s.sell_rows)} / ${number(s.buy_rows)}</td><td>${number(s.stock_min)}–${number(s.stock_max)}<small>avg ${number(s.stock_average)}</small></td><td>${number(s.buy_modifier)}% / ${number(s.sell_modifier)}%</td><td>${number(s.nearest_hub_jumps)}</td></tr>`).join('')}</tbody></table></div>`;
    $('distributionPreview').innerHTML=`<div class="distribution-results"><h3>Distribution Preview · ${p.valid?'Ready':'Safety errors'}</h3><p class="muted">Exact predicted rows. No SQLite written.</p><div class="metric-grid">${metric('Eligible NPC stations',p.eligible_npc_stations)}${metric('Canonical hubs',p.canonical_hubs)}${metric('Selected secondary stations',p.selected_secondary_stations)}${metric('Total selected stations',p.selected_stations)}${metric('Sell rows',p.sell_rows)}${metric('Buy rows',p.buy_rows)}${metric('Total seed rows',p.total_seed_rows)}</div>
      ${(p.warnings||[]).map(w=>`<p class="hint">${esc(w)}</p>`).join('')}${(p.errors||[]).map(e=>{const buy=p.stations.find(s=>s.station_id===e.buy_station_id||s.station_id===e.station_id),sell=p.stations.find(s=>s.station_id===e.sell_station_id||s.station_id===e.station_id);const item=state.previewMap.get(String(e.type_id));const detail=e.reason==='INVALID_ROUNDED_PRICE'?`${esc(title(e.side||'quote'))} ${price(e.price)} at ${esc(buy?.name||sell?.name||'—')} · Base ${price(e.canonical_price)} × ${number(e.modifier_percent)}%`:`Buy ${price(e.highest_buy??e.buy)} at ${esc(buy?.name||'—')} · Sell ${price(e.lowest_sell??e.sell)} at ${esc(sell?.name||'—')}`;return `<div class="distribution-error"><strong>${esc(item?.name||'Item '+e.type_id)} · ${esc(title(e.reason))}</strong><p>${detail}</p></div>`;}).join('')}
      ${(p.crossing_warnings||[]).length?`<details><summary>Buy / Sell review examples · ${number(p.local_crossings)} local pairs, ${number(p.global_crossings)} types across stations</summary>${p.crossing_warnings.map(x=>`<p>${esc(state.previewMap.get(String(x.type_id))?.name||x.type_id)}: Buy ${price(x.buy??x.highest_buy)} · Sell ${price(x.sell??x.lowest_sell)}. Prices retained; building is allowed.</p>`).join('')}</details>`:''}
      ${p.price_floor?.adjusted_quotes?`<details><summary>Minimum-price examples · ${number(p.price_floor.adjusted_quotes)} quotes at 0.01 ISK</summary>${(p.price_floor.samples||[]).map(s=>`<p>${esc(s.name)} · ${esc(title(s.side))} at ${esc(s.station_name)}: ${price(s.canonical_price)} × ${number(s.modifier_percent)}% → ${price(s.price)}</p>`).join('')}</details>`:''}
      ${(p.tiers||[]).map(t=>`<div class="distribution-tier"><strong>${esc(title(t.tier))} · ${number(t.stations)} stations</strong><p>Assortment ${number(t.assortment.min)}–${number(t.assortment.max)}% · avg ${number(t.assortment.average)}%<br>Stock ${number(t.stock.min)}–${number(t.stock.max)} · avg ${number(t.stock.average)}<br>Buy ${number(t.buy_modifier.min)}–${number(t.buy_modifier.max)}% · Sell ${number(t.sell_modifier.min)}–${number(t.sell_modifier.max)}%</p></div>`).join('')}
      <details open><summary>Station books</summary>${stationTable(p.stations)}</details><details><summary>Largest books</summary>${stationTable(p.largest_books)}</details><details><summary>Smallest books</summary>${stationTable(p.smallest_books)}</details><details><summary>Reproducibility</summary><p>Distribution seed: ${esc(state.distribution.seed)}</p><p>Plan SHA256: ${esc(p.plan_sha256)}</p><p>Row fingerprint: ${esc(p.row_fingerprint)}</p></details></div>`;
  }
  async function previewDistribution(event){
    event?.preventDefault();if(state.busy||!state.distribution)return;
    const before=JSON.stringify(state.distribution);readDistributionForm();
    if(before!==JSON.stringify(state.distribution)&&!builtIn())markDirty();
    const revision=state.revision;state.busy=true;refreshChrome();notice('Resolving Distribution Preview…');
    try{const r=await post('/distribution/preview',{policy:state.policy,preset:state.preset,distribution:state.distribution});
      if(revision!==state.revision)return;
      state.distribution=r.distribution;state.distributionPreview=r;renderDistribution();notice(r.valid?`Distribution ready: ${number(r.selected_stations)} NPC stations, ${number(r.total_seed_rows)} exact seed rows.`:`Distribution blocked: ${number(r.error_count)} price safety errors.`,!r.valid);
    }catch(error){state.distributionPreview=null;renderDistributionPreview();notice(`Distribution preview failed: ${error.message}`,true);}
    finally{state.busy=false;refreshChrome();}
  }
  function bindDistribution(){
    $('distributionCopy').onclick=()=>$('duplicateBtn').click();
    $('candidateDistributionPreview').onclick=()=>{showPage('dashboard');$('distributionCard').scrollIntoView({block:'start'});previewDistribution();};
    $('distributionForm').onsubmit=previewDistribution;
    for(const id of ['distributionMode','distAssortmentMin','distAssortmentMax','distStockMin','distStockMax','distBuyMin','distBuyMax','distSellMin','distSellMax','distRegionMin','distRegionMax','distRemoteness','distSeed','distCustomSelection','distCustomRegions']){
      const update=()=>{if(builtIn())return;const before=JSON.stringify(state.distribution);readDistributionForm();if(before===JSON.stringify(state.distribution))return;markDirty();if(['distributionMode','distCustomSelection'].includes(id))renderDistribution();else renderDistributionOverride();};
      $(id).addEventListener('change',update);if($(id).tagName==='INPUT')$(id).addEventListener('input',update);
    }
    $('distNewSeed').onclick=()=>{if(builtIn())return;const bytes=new Uint32Array(2);crypto.getRandomValues(bytes);$('distSeed').value=Array.from(bytes,x=>x.toString(16).padStart(8,'0')).join('');readDistributionForm();markDirty();};
    $('distCustomHubs').addEventListener('change',()=>{readDistributionForm();markDirty();renderDistribution();});
    $('distOverrideScope').onchange=renderDistributionOverride;
    $('distOverrideApply').onclick=()=>{if(builtIn())return;const o={};if($('distOverrideAssortment').checked)o.assortment={min:Number($('distOverrideAssortmentMin').value),max:Number($('distOverrideAssortmentMax').value)};if($('distOverrideStock').checked)o.stock={min:Number($('distOverrideStockMin').value),max:Number($('distOverrideStockMax').value)};
      const id=$('distOverrideScope').value;if(Object.keys(o).length)state.distribution.overrides[id]=o;else delete state.distribution.overrides[id];markDirty();renderDistributionOverride();};
    $('distOverrideReset').onclick=()=>{if(builtIn())return;delete state.distribution.overrides[$('distOverrideScope').value];markDirty();renderDistributionOverride();};
    $('distStationSearchBtn').onclick=findDistributionStations;
    $('distStationSearch').onkeydown=event=>{if(event.key==='Enter'){event.preventDefault();findDistributionStations();}};
    $('distStationResults').addEventListener('change',event=>{const el=event.target.closest('[data-dist-station]');if(!el||builtIn())return;const id=Number(el.dataset.distStation);let ids=state.distribution.custom.station_ids.filter(x=>x!==id);if(el.checked)ids.push(id);state.distribution.custom.station_ids=ids;markDirty();renderChosenStations();});
    $('distChosenStations').addEventListener('click',event=>{const el=event.target.closest('[data-dist-remove]');if(!el||builtIn())return;state.distribution.custom.station_ids=state.distribution.custom.station_ids.filter(x=>x!==Number(el.dataset.distRemove));markDirty();renderChosenStations();});
  }

  function refreshBlueprintControls(){
    const locked=builtIn()||state.loadingPreset||state.busy||!state.policy;
    $('blueprintEditBtn').disabled=locked;$('blueprintResetBtn').disabled=locked||!state.blueprintData?.rule;
    $('blueprintCopyBtn').classList.toggle('hidden',!builtIn());$('blueprintCopyBtn').disabled=state.loadingPreset||state.busy;
  }
  async function loadBlueprintSchema(){
    state.blueprintSchema=await api('/blueprints/schema');
    $('blueprintFamily').innerHTML=state.blueprintSchema.families.map(f=>`<option value="${esc(f.id)}">${esc(f.label)} (${number(f.count)})</option>`).join('');
    $('blueprintCensus').textContent=`${number(state.blueprintSchema.total)} published blueprints · ${number(state.blueprintSchema.no_market_group)} without TQ marketGroupID · Advanced catalog only`;
    $('blueprintWarning').textContent=state.blueprintSchema.warning;
  }
  async function loadBlueprintCatalog(){
    if(!state.policy||!state.blueprintSchema)return;
    const serial=++state.blueprintSerial,revision=state.revision;
    $('blueprintBody').innerHTML='<tr><td colspan="6" class="empty">Reading published blueprints…</td></tr>';
    try{const r=await post('/blueprints/catalog',{policy:state.policy,preset:state.preset,family:$('blueprintFamily').value,q:$('blueprintSearch').value.trim(),offset:state.blueprintPage*50,limit:50});
      if(serial!==state.blueprintSerial||revision!==state.revision)return;state.blueprintData=r;
      const rule=r.rule,source=s=>s?`${labels[s.source]||title(s.source)} × ${s.multiplier}`:'Disabled';
      $('blueprintFamilyState').textContent=rule?`Family rule: ${availabilityLabel(rule.sides)} · Buy: ${source(rule.buy)} · Sell: ${source(rule.sell)} · Priority ${rule.priority}`:'No family override. Existing preset rules and exact overrides apply; no new blueprints are enabled automatically.';
      $('blueprintBody').innerHTML=r.items.map(b=>{const p=b.preview||{},policy=b.resolution?.policy;return `<tr class="clickable" data-blueprint-type="${b.type_id}"><td><span class="item-name">${esc(b.name)}</span><span class="item-id">${b.type_id}</span></td><td>${b.product_names.map(esc).join('<br>')||'Special / no manufacturing product'}</td><td>${b.market_group_id==null?'<span class="badge warn">No marketGroupID</span>':'Listed'}</td><td>${esc(availabilityLabel(policy?.sides||'unseeded'))}</td><td class="numeric">${price(p.buy_price)} / ${price(p.sell_price)}</td><td>${p.unresolved_reason?`<span class="badge warn" title="${esc(p.unresolved_reason)}">Unresolved source</span>`:'<span class="badge warn">Instance warning</span>'}</td></tr>`;}).join('')||'<tr><td colspan="6" class="empty">No published blueprints match.</td></tr>';
      const first=state.blueprintPage*50+1,last=Math.min((state.blueprintPage+1)*50,r.total);
      $('blueprintPageLabel').textContent=r.total?`${number(first)}–${number(last)} of ${number(r.total)}`:'No results';
      $('blueprintPrev').disabled=state.blueprintPage===0;$('blueprintNext').disabled=last>=r.total;refreshBlueprintControls();
    }catch(error){$('blueprintBody').innerHTML=`<tr><td colspan="6" class="empty">${esc(error.message)}</td></tr>`;notice(`Blueprint catalog failed: ${error.message}`,true);}
  }
  function openBlueprintEditor(){
    if(builtIn())return notice('Choose Edit a Copy before changing family settings.');
    const id=$('blueprintFamily').value,f=state.blueprintSchema.families.find(f=>f.id===id);
    openEditor('blueprint');state.editor.family=id;
    const rule=state.policy.rules.find(r=>r.id===f.rule_id);
    if(rule)fillRule(rule);
    $('settingsMode').value='inline';$('editorEyebrow').textContent='ADVANCED BLUEPRINT FAMILY';$('editorTitle').textContent=f.label;
    $('editorIntro').textContent=state.blueprintSchema.warning;
    $('editorMatch').textContent=`Matches all ${number(f.count)} published family types, including ${number(f.no_market_group)} without marketGroupID. Simple Mode membership stays unchanged.`;
    updateEditorVisibility();
  }
  function bindBlueprints(){
    $('blueprintCopyBtn').onclick=()=>$('duplicateBtn').click();
    $('blueprintFamily').onchange=()=>{state.blueprintPage=0;loadBlueprintCatalog();};
    $('blueprintSearchBtn').onclick=()=>{state.blueprintPage=0;loadBlueprintCatalog();};
    $('blueprintSearch').onkeydown=e=>{if(e.key==='Enter'){e.preventDefault();state.blueprintPage=0;loadBlueprintCatalog();}};
    $('blueprintPrev').onclick=()=>{state.blueprintPage=Math.max(0,state.blueprintPage-1);loadBlueprintCatalog();};
    $('blueprintNext').onclick=()=>{state.blueprintPage++;loadBlueprintCatalog();};
    $('blueprintEditBtn').onclick=openBlueprintEditor;
    $('blueprintResetBtn').onclick=async()=>{if(builtIn())return;const id=state.blueprintSchema.families.find(f=>f.id===$('blueprintFamily').value).rule_id;state.policy.rules=state.policy.rules.filter(r=>r.id!==id);markDirty();await syncQuick();await previewData(false);loadBlueprintCatalog();};
    $('blueprintBody').onclick=e=>{const row=e.target.closest('[data-blueprint-type]');if(row)selectType(row.dataset.blueprintType);};
  }


  function bindPortable(){
    $('exportPresetBtn').onclick=()=>{
      $('exportPresetName').value=state.displayName;$('exportPresetDescription').value=state.description;$('exportPresetAuthor').value=state.author;
      $('exportPresetError').classList.add('hidden');$('exportPresetDialog').showModal();
    };
    $('exportPresetForm').onsubmit=async event=>{
      event.preventDefault();if(state.busy)return;
      state.busy=true;refreshChrome();$('exportPresetConfirm').disabled=true;$('exportPresetConfirm').textContent='Preparing JSON…';
      try{
        const result=await post('/presets/export',{policy:state.policy,preset:state.preset,distribution:state.distribution,
          name:$('exportPresetName').value,description:$('exportPresetDescription').value,author:$('exportPresetAuthor').value});
        const url=URL.createObjectURL(new Blob([result.json],{type:'application/json;charset=utf-8'}));
        const link=document.createElement('a');link.href=url;link.download=result.filename;document.body.append(link);link.click();link.remove();setTimeout(()=>URL.revokeObjectURL(url),10000);
        $('exportPresetDialog').close();notice('Preset exported. The JSON includes the current draft and Distribution; your saved preset is unchanged.');
      }catch(error){$('exportPresetError').textContent=error.message;$('exportPresetError').classList.remove('hidden');}
      finally{state.busy=false;refreshChrome();$('exportPresetConfirm').disabled=false;$('exportPresetConfirm').textContent='Download JSON';}
    };
    const resetImport=()=>{state.importSerial++;state.importDraft=null;$('importPresetConfirm').disabled=true;$('importPresetSummary').textContent='';$('importPresetStatus').textContent='';$('importPresetError').classList.add('hidden');};
    $('importPresetBtn').onclick=()=>{resetImport();$('importPresetFile').value='';$('importPresetDialog').showModal();};
    $('importPresetDialog').addEventListener('close',resetImport);
    $('importPresetDialog').addEventListener('cancel',event=>{if(state.importCreating)event.preventDefault();});
    $('importPresetFile').onchange=async()=>{
      resetImport();const file=$('importPresetFile').files[0];if(!file)return;const serial=state.importSerial;
      $('importPresetStatus').textContent='Validating the preset and local dependencies…';
      try{
        if(file.size>4*1024*1024)throw new Error('Preset file exceeds the 4 MiB import limit.');
        const json=await file.text(),result=await post('/presets/import/preview',{json});if(serial!==state.importSerial)return;
        const s=result.summary,q=s.item_policy||{};
        state.importDraft={json,sha256:result.sha256};
        $('importPresetSummary').textContent=[s.name,`Base: ${presetLabel(s.base_preset)} · portable format ${s.format_version}`,
          s.description?`Description: ${s.description}`:'',s.author?`Author: ${s.author}`:'',
          `Profiles: ${s.profiles} · Rules: ${s.rules} · Expert exact overrides: ${s.expert_exact_overrides}`,
          `Quick Setup / Advanced: ${s.quick_setup_scopes.length} customized groups`,
          s.quick_setup_scopes.map(x=>x.label).join(', '),
          `Buy: ${number(q.buy)} · Sell: ${number(q.sell)} · Both: ${number(q.both_sides)}`,
          `Unresolved / review: ${number(q.unresolved)}`,
          `Configured special/non-market types: ${number(s.configured_nonmarket_types)}`,
          `Distribution: ${title(s.distribution_mode)} · ${number(s.distribution.selected_stations)} stations · seed: ${s.distribution_seed}`,
          `Sources: ${s.required_price_sources.map(x=>labels[x]||x).join(', ')}`,
          ...s.warnings].filter(Boolean).join('\n');
        $('importPresetStatus').textContent='Validated. Nothing has been saved yet.';$('importPresetConfirm').disabled=false;
      }catch(error){if(serial!==state.importSerial)return;$('importPresetStatus').textContent='Import unavailable';$('importPresetError').textContent=error.message;$('importPresetError').classList.remove('hidden');}
    };
    $('importPresetConfirm').onclick=async()=>{
      if(!state.importDraft||state.busy)return;
      const current=state.importDraft;state.busy=true;state.importCreating=true;refreshChrome();$('importPresetConfirm').disabled=true;$('importPresetConfirm').textContent='Creating…';$('importPresetFile').disabled=true;
      $('importPresetStatus').textContent='Creating a new user preset…';$('importPresetDialog').querySelectorAll('[data-close]').forEach(x=>x.disabled=true);
      try{
        const result=await post('/presets/import',{json:current.json,preview_sha256:current.sha256});
        $('importPresetDialog').close();await loadSaved(result.id);
        notice(`Imported ${result.summary.name} as a new user preset. No market was built or deployed.`);
      }catch(error){$('importPresetError').textContent=error.message;$('importPresetError').classList.remove('hidden');$('importPresetConfirm').disabled=false;}
      finally{state.busy=false;state.importCreating=false;refreshChrome();$('importPresetFile').disabled=false;$('importPresetConfirm').textContent='Create User Preset';$('importPresetDialog').querySelectorAll('[data-close]').forEach(x=>x.disabled=false);}
    };
  }

  async function init(){
    bind();bindSimple();bindBlueprints();bindPortable();refreshChrome();
    try{const health=await api('/health');$('apiStatus').textContent=health.ok?'● Connected · 127.0.0.1':'Backend unavailable';$('apiStatus').classList.toggle('offline',!health.ok);}
    catch(error){$('apiStatus').textContent='Backend offline';$('apiStatus').classList.add('offline');notice(`Cannot connect to local Workbench: ${error.message}`,true);return;}
    bindDistribution();
    await loadDistributionSchema();
    await loadSchema();
    await loadPreset('GENERAL_TQ');
  }
  init();
})();
