'use strict';
let reviewId=null, reviewData=null, reviewDir='', reviewPicked=new Set(), reviewLoading=false, reviewStatus='', previewAbort=null;
const typeNames={image:'图片',audio:'音频',video:'视频',text:'文本',archive:'压缩包',other:'文件',folder:'文件夹'};
function fileKind(name) {
  const ext=name.toLowerCase().split('.').pop();
  if(['zip','rar','7z','tar','gz','bz2','xz','zst','001'].includes(ext))return 'archive';
  if(['jpg','jpeg','png','gif','webp','avif','bmp'].includes(ext))return 'image';
  if(['mp3','wav','flac','ogg','oga','m4a','aac'].includes(ext))return 'audio';
  if(['mp4','m4v','webm','mov','mkv'].includes(ext))return 'video';
  if(['txt','md','json','csv','log','srt','ass','lrc','ini','toml','yaml','yml','xml','html','htm','svg','nfo','url'].includes(ext))return 'text';
  return 'other';
}
function within(root,path){return path===root||path.startsWith(root+'/');}
function picked(entry){return [...reviewPicked].some(path=>within(path,entry.path));}
function chosenFiles(){return (reviewData?.entries||[]).filter(e=>!e.is_dir&&picked(e));}
function stopPreview(){
  previewAbort?.abort();previewAbort=null;
  for(const player of $('preview-content').querySelectorAll('video,audio')){player.pause();player.removeAttribute('src');player.load();}
  for(const image of $('preview-content').querySelectorAll('img'))image.removeAttribute('src');
  $('preview-content').replaceChildren();$('preview-info').replaceChildren();
}
function closeShare(){stopPreview();reviewId=null;reviewData=null;reviewPicked.clear();$('share-dialog').close();}
$('close-share').onclick=closeShare;$('share-dialog').addEventListener('cancel',()=>{stopPreview();reviewId=null;reviewData=null;});
async function openShare(id){
  stopPreview();reviewId=id;reviewData=null;reviewPicked.clear();reviewStatus='';reviewDir='';
  $('share-dialog').classList.remove('cloud-only');$('share-title').textContent='分享内容';$('share-search').value='';$('share-type').value='all';$('share-scope').value='folder';
  $('share-explorer').hidden=true;$('share-capacity').replaceChildren();$('share-state').textContent='正在读取分享…';
  if(!$('share-dialog').open)$('share-dialog').showModal();updateReviewSelection();
  try{
    const shares=await api('/shares');const share=shares.find(s=>s.id===id);
    if(share&&['queued','ready','failed','cancelled'].includes(share.status)&&!share.has_contents)await api('/shares/'+id+'/retry',{method:'POST'});
    await loadShareContents();
  }catch(e){$('share-state').textContent=e.message;}
}
async function loadShareContents(){
  if(!reviewId||reviewLoading)return;reviewLoading=true;const id=reviewId;
  try{
    const data=await api('/shares/'+id+'/contents');if(reviewId!==id)return;
    const first=!reviewData?.entries?.length;reviewData=data;reviewStatus=data.status;
    $('share-subtitle').textContent=data.entries.length?`${data.entries.filter(e=>!e.is_dir).length} 个文件 · ${bytes(data.entries.filter(e=>!e.is_dir).reduce((s,e)=>s+e.size,0))} · 勾选后再下载`:'先准备内容，确认后再开始下载';
    renderCapacity(data.capacity);
    const ready=['ready','queued'].includes(data.status)&&data.entries.length;
    $('share-explorer').hidden=!ready;$('share-state').replaceChildren();
    if(!ready){
      $('share-state').append(text('p',data.message||'正在解析分享内容…'));
      if(data.status==='awaiting_transfer')$('share-state').append(button('转存并查看文件',async()=>{
        if(data.capacity?.known!==true&&!confirm('暂时无法读取网盘容量。仍要尝试转存以准备预览吗？'))return;
        try{await api('/shares/'+id+'/prepare',{method:'POST',body:JSON.stringify({allow_unknown_capacity:data.capacity?.known!==true})});await loadShareContents();}catch(e){toast(e.message);}
      },'primary'));
      if(['insufficient_capacity','awaiting_transfer','failed'].includes(data.status)){
        $('share-state').append(button('管理网盘文件',()=>{const s=accounts.find(a=>a.id===lastShares.find(s=>s.id===id)?.provider);closeShare();setView('browse');if(s?.mount_path)browse(s.mount_path,1,true);}));
        $('share-state').append(button('重新检查',async()=>{try{await api('/shares/'+id+'/retry',{method:'POST'});await loadShareContents();}catch(e){toast(e.message);}}));
      }
      if(data.status==='waiting_login')$('share-state').append(button('连接网盘',()=>{const s=lastShares.find(s=>s.id===id);if(s)openAccount(s.provider);},'primary'));
    }else{
      if(first)reviewDir=data.roots.length===1&&data.entries.find(e=>e.path===data.roots[0])?.is_dir?data.roots[0]:'';
      renderReviewFiles();
      if(first){$('preview-title').textContent='文件预览';$('preview-content').replaceChildren(text('div','选择一个文件，查看图片、音频、视频或文本预览。','preview-placeholder'));}
    }
    updateReviewSelection();
  }catch(e){if(reviewId===id)$('share-state').textContent=e.message;}finally{reviewLoading=false;}
}
let lastShares=[];
function refreshShareDialog(shares){lastShares=shares;if(reviewId&&$('share-dialog').open){const s=shares.find(s=>s.id===reviewId);if(!s){closeShare();return;}if(s.status!==reviewStatus)loadShareContents();}}
function renderCapacity(c){
  $('share-capacity').replaceChildren();if(!c?.requires_transfer)return;
  const box=text('section','','capacity-box');box.append(text('strong','网盘容量 · 转存前估算'));
  const grid=text('div','','capacity-grid');
  for(const [label,value]of [['总容量',c.total],['当前已用',c.used],['现有可用',c.free],['预计转存占用',c.required],['转存后预计可用',c.after]]){
    const metric=text('div');metric.append(text('small',label),text('b',value==null?'暂未获取':bytes(value)));grid.append(metric);
  }
  box.append(grid,text('small',c.enough===false?'剩余空间不足。请先管理网盘文件，再重新检查容量。':c.known?'转存由你确认；云端去重或并发操作可能使实际剩余容量不同。':'暂时无法获取网盘容量，可重新检查。'));
  box.classList.toggle('capacity-low',c.enough===false);$('share-capacity').append(box);
}
function visitReview(path){reviewDir=path;$('share-scope').value='folder';$('share-search').value='';$('share-type').value='all';renderReviewFiles();}
function reviewVisible(){
  const search=$('share-search').value.toLocaleLowerCase(),kind=$('share-type').value;
  const all=$('share-scope').value==='all'||!!search||kind!=='all';
  return (reviewData?.entries||[]).filter(e=>{
    if(all&&e.is_dir)return false;
    if(!all&&!(reviewDir?e.path.slice(0,e.path.lastIndexOf('/'))===reviewDir:reviewData.roots.includes(e.path)))return false;
    return (!search||e.name.toLocaleLowerCase().includes(search))&&(kind==='all'||fileKind(e.name)===kind);
  }).sort((a,b)=>Number(b.is_dir)-Number(a.is_dir)||a.name.localeCompare(b.name,'zh-CN'));
}
function toggleReview(entry,on){
  if(!on){reviewPicked=new Set(chosenFiles().filter(e=>!within(entry.path,e.path)).map(e=>e.path));}
  else{for(const p of reviewPicked)if(within(entry.path,p))reviewPicked.delete(p);reviewPicked.add(entry.path);}
}
function passwordNotes(values){
  const wrap=text('div','','password-notes');
  for(const value of values||[]){const pill=text('span','解压密码：'+value,'password-pill');pill.append(button('复制',async event=>{event?.stopPropagation();try{if(navigator.clipboard)await navigator.clipboard.writeText(value);else{const area=document.createElement('textarea');area.value=value;area.className='copy-buffer';document.body.append(area);area.select();const ok=document.execCommand('copy');area.remove();if(!ok)throw Error('copy');}toast('解压密码已复制');}catch{toast('请选中密码文字复制');}}));wrap.append(pill);}
  return wrap;
}
function renderReviewFiles(){
  if(!reviewData)return;
  const folders=$('share-folders');folders.replaceChildren(text('small','文件夹','review-nav-label'));
  const root=reviewData.roots.length===1&&reviewData.entries.find(e=>e.path===reviewData.roots[0])?.is_dir?reviewData.roots[0]:'';
  const home=button('▦ 全部内容',()=>visitReview(root));home.classList.toggle('current',reviewDir===root);folders.append(home);
  for(const dir of reviewData.entries.filter(e=>e.is_dir&&e.path!==root)){
    const b=button('▰ '+dir.name,()=>visitReview(dir.path));b.title=dir.name;b.classList.toggle('current',dir.path===reviewDir);folders.append(b);
  }
  const crumbs=$('share-crumbs');crumbs.replaceChildren(button('分享内容',()=>visitReview(root)));
  const chain=reviewData.entries.filter(e=>e.is_dir&&e.path!==root&&within(e.path,reviewDir)).sort((a,b)=>a.path.length-b.path.length);
  for(const dir of chain)crumbs.append(text('span','/'),button(dir.name,()=>visitReview(dir.path)));
  $('share-files').replaceChildren();const visible=reviewVisible();
  for(const entry of visible){
    const tr=document.createElement('tr');tr.classList.toggle('review-picked',picked(entry));
    const td=text('td','','check'),check=document.createElement('input');check.type='checkbox';check.checked=picked(entry);check.setAttribute('aria-label','选择 '+entry.name);
    check.onchange=()=>{toggleReview(entry,check.checked);renderReviewFiles();};td.append(check);
    const name=text('td'),line=text('div','','review-filename');const kind=entry.is_dir?'folder':fileKind(entry.name);
    line.append(text('span',({folder:'▰',image:'▧',audio:'♫',video:'▷',text:'≡',archive:'▣',other:'◇'})[kind],'kind-icon kind-'+kind));
    const label=button(entry.name,()=>entry.is_dir?visitReview(entry.path):showFilePreview(entry.path,entry.name,'/shares/'+reviewId+'/preview'));label.title=entry.name;line.append(label);name.append(line);
    const tags=text('div','','file-badges');tags.append(text('span',typeNames[kind]));
    if(entry.ad_reason){const flag=text('span','可能是广告','ad-badge');flag.title=entry.ad_reason;tags.append(flag);}
    name.append(tags);if(entry.passwords?.length)name.append(passwordNotes(entry.passwords));
    const action=text('td','','review-action');action.append(button(entry.is_dir?'打开':'预览',()=>entry.is_dir?visitReview(entry.path):showFilePreview(entry.path,entry.name,'/shares/'+reviewId+'/preview')));
    tr.append(td,name,text('td',bytes(entry.size),'review-size'),action);$('share-files').append(tr);
  }
  $('share-empty').hidden=visible.length>0;const count=visible.filter(picked).length;
  $('share-check-all').checked=!!visible.length&&count===visible.length;$('share-check-all').indeterminate=count>0&&count<visible.length;
  updateReviewSelection();
}
function updateReviewSelection(){
  const chosen=chosenFiles(),ads=chosen.filter(e=>e.ad_reason),ready=reviewData&&['ready','queued'].includes(reviewData.status);
  $('share-selection').textContent=chosen.length?`已选 ${chosen.length} 个文件 · ${bytes(chosen.reduce((n,e)=>n+e.size,0))}${ads.length?' · 含 '+ads.length+' 个疑似广告':''}`:'尚未选择文件';
  $('share-download').disabled=!ready||!chosen.length||chosen.length>1000;
  $('share-download').textContent=chosen.length>1000?'一次最多 1000 个文件':'下载所选 →';
  $('share-select-entire').disabled=!ready;$('share-select-none').disabled=!reviewPicked.size;
}
$('share-check-all').onchange=()=>{const on=$('share-check-all').checked;for(const e of reviewVisible())toggleReview(e,on);renderReviewFiles();};
$('share-select-none').onclick=()=>{reviewPicked.clear();renderReviewFiles();};
$('share-select-entire').onclick=()=>{reviewPicked=new Set(reviewData.roots);renderReviewFiles();};
for(const id of ['share-search','share-type','share-scope'])$(id).addEventListener(id==='share-search'?'input':'change',renderReviewFiles);
async function startReviewDownload(allowAds=false){
  $('share-download').disabled=true;
  try{
    const result=await api('/shares/'+reviewId+'/download',{method:'POST',body:JSON.stringify({paths:[...reviewPicked],allow_ads:allowAds})});
    toast(`已将 ${result.jobs.length} 个文件加入同一下载组`);closeShare();setView('tasks');await loadJobs();await loadShares();
  }catch(e){toast(e.message);updateReviewSelection();}
}
$('share-download').onclick=()=>{
  const ads=chosenFiles().filter(e=>e.ad_reason);
  if(!ads.length){startReviewDownload();return;}
  $('ad-files').replaceChildren(...ads.slice(0,8).map(e=>text('li',e.name+' · '+e.ad_reason)));
  if(ads.length>8)$('ad-files').append(text('li',`另有 ${ads.length-8} 个疑似广告文件`));
  $('ad-dialog').showModal();
};
$('ad-back').onclick=()=>$('ad-dialog').close();
$('ad-remove').onclick=()=>{reviewPicked=new Set(chosenFiles().filter(e=>!e.ad_reason).map(e=>e.path));$('ad-dialog').close();renderReviewFiles();toast('已取消选择疑似广告文件');};
$('ad-keep').onclick=()=>{$('ad-dialog').close();startReviewDownload(true);};
async function showFilePreview(path,name,endpoint){
  stopPreview();$('preview-title').textContent=name;const kind=fileKind(name),url='/api'+endpoint+'?'+new URLSearchParams({path});
  const entry=reviewData?.entries.find(e=>e.path===path);$('preview-info').append(text('small',typeNames[kind]+(entry?' · '+bytes(entry.size):'')));
  if(entry?.ad_reason)$('preview-info').append(text('p','可能是广告：'+entry.ad_reason,'ad-badge'));
  if(entry?.passwords.length)$('preview-info').append(passwordNotes(entry.passwords));
  if(kind==='image'){
    const img=document.createElement('img');img.alt=name;img.src=url;img.onerror=()=>{$('preview-content').replaceChildren(text('p','图片暂时无法预览，可下载原文件查看。','preview-placeholder'));};$('preview-content').append(img);
  }else if(kind==='video'||kind==='audio'){
    const player=document.createElement(kind);player.controls=true;player.preload='metadata';player.src=url;
    player.onerror=()=>{$('preview-content').append(text('p','浏览器不支持此格式或网盘暂不可用，可下载原文件播放。','preview-placeholder'));};
    $('preview-content').append(player);$('preview-info').append(text('small','支持的格式可在线播放和拖动进度，不会创建下载任务。'));
  }else if(kind==='text'){
    const controller=new AbortController();previewAbort=controller;$('preview-content').append(text('div','正在加载文本…','preview-placeholder'));
    try{
      const response=await fetch(url,{signal:controller.signal,headers:{Range:'bytes=0-262143'}});
      if(!response.ok){const err=await response.json().catch(()=>({}));throw Error(err.error||'文本预览失败');}
      const data=new Uint8Array(await response.arrayBuffer());if(controller.signal.aborted)return;
      let content;try{content=new TextDecoder('utf-8',{fatal:true}).decode(data,{stream:true});}catch{content=new TextDecoder('gb18030').decode(data);}
      $('preview-content').replaceChildren(text('pre',content,'text-preview'));if(entry?.size>262144)$('preview-info').append(text('small','仅预览前 256 KB，下载可获得完整文本。'));
    }catch(e){if(e.name!=='AbortError')$('preview-content').replaceChildren(text('p',e.message,'preview-placeholder'));}
  }else $('preview-content').append(text('div','此格式可下载原文件后打开。','preview-placeholder'));
}
function previewCloud(path,name){
  stopPreview();reviewId=null;reviewData=null;reviewPicked.clear();$('share-dialog').classList.add('cloud-only');$('share-title').textContent='文件预览';$('share-subtitle').textContent='在线查看网盘文件';$('share-capacity').replaceChildren();$('share-state').replaceChildren();$('share-explorer').hidden=false;
  if(!$('share-dialog').open)$('share-dialog').showModal();showFilePreview(path,name,'/preview');
}
