"use strict";
async function loadTools(){const settings=await api('/settings');$('download-limit').value=settings.download_limit/1024;$('upload-limit').value=settings.upload_limit/1024;$('seed-ratio').value=settings.seed_ratio;$('seed-minutes').value=settings.seed_minutes;const s=await api('/transfers/status');$('engine-state').textContent=s.available?'aria2 已连接 · '+s.protocols.join(' / ')+(s.connections_per_file?' · 每文件最多 '+s.connections_per_file+' 连接':''):'链接引擎未启动，请使用 Docker 部署';}
function headersInput(){const result={};for(const line of $('link-headers').value.split('\n').map(s=>s.trim()).filter(Boolean)){const i=line.indexOf(':');if(i<=0)throw Error('请求头请使用 名称: 值 格式');result[line.slice(0,i).trim()]=line.slice(i+1).trim();}return result;}
function downloadInputs(raw){
  const urls=raw.match(/(?:magnet:\?|(?:https?|ftp|sftp):\/\/)[^\s<>"，。；）】]+/gi)||[];
  if(!urls.length)throw Error('请粘贴下载链接或磁力链接，也可拖入 .torrent 文件');
  if(urls.length>100)throw Error('每批最多添加 100 条链接');
  return urls.map(url=>{const u=new URL(url);const host=u.hostname.toLowerCase();const share=u.protocol==='https:'&&(['pan.baidu.com','pan.quark.cn','115.com','115cdn.com','anxia.com','pan.xunlei.com'].includes(host)&&u.pathname.startsWith('/s/')||['drive.google.com','1drv.ms','onedrive.live.com'].includes(host)||host.endsWith('.sharepoint.com')&&u.pathname.startsWith('/:'));return {url:share&&urls.length===1?raw:url,share,github:githubRepository(u)};});
}
function githubRepository(u){
  if(u.protocol!=='https:'||u.hostname!=='github.com'||u.port||u.username||u.password)return null;
  const parts=u.pathname.replace(/\/+$/,'').split('/').slice(1),owner=parts[0]||'',repo=(parts[1]||'').replace(/\.git$/,'');
  const valid=s=>/^[A-Za-z0-9_.-]{1,100}$/.test(s)&&s!=='.'&&s!=='..';
  if(!valid(owner)||!valid(repo))return null;
  if(!(parts.length===2||parts[2]==='releases'&&(parts.length===3||parts.length===4&&parts[3]==='latest'||parts.length===5&&parts[3]==='tag'&&parts[4])))return null;
  return {owner,repo,url:'https://github.com/'+owner+'/'+repo};
}
async function submitDownloadInput(raw){
  const inputs=downloadInputs(raw);
  if(inputs.some(input=>input.github)){if(inputs.length!==1)throw Error('请每次浏览一个 GitHub 仓库或 Release，附件可批量勾选下载');await openGithub(inputs[0].url);return;}
  const headers=headersInput(),group_id=batchUuid(),shares=[];
  let added=0;
  try{for(const input of inputs){if(input.share){shares.push(await api('/shares',{method:'POST',body:JSON.stringify({url:input.url,passcode:$('share-code').value.trim()})}));}else{await api('/transfers',{method:'POST',body:JSON.stringify({uri:input.url,headers,group_id,group_name:$('link-batch').value||'链接批次',create_folder:$('link-create-folder').checked})});}added++;}}
  catch(e){throw Error((added?'已接收 '+added+' 项；':'')+e.message);}
  $('share-url').value='';$('share-code').value='';$('link-headers').value='';$('share-create-folder').checked=$('link-create-folder').checked;await loadJobs();await loadShares();
  if(shares.length===1&&inputs.length===1){const account=accounts.find(a=>a.id===shares[0].provider);if(account&&!account.connected)await openAccount(account.id);else await openShare(shares[0].id);}
  else if(shares.length)setView('browse');else setView('tasks');
  toast('已接收 '+added+' 个链接'+(shares.length?'，网盘分享请浏览并选择内容':''));
}
$('share-form').onsubmit=async event=>{event.preventDefault();$('share-submit').disabled=true;try{await submitDownloadInput($('share-url').value.trim());}catch(e){toast(e.message);}finally{$('share-submit').disabled=false;}};
function appendTransferActions(job,actions){if(['selecting','paused'].includes(job.status)&&['torrent','magnet'].includes(job.kind)&&job.files.length)actions.append(button('选择文件',()=>openTorrent(job),'primary'));for(const file of job.files||[])if(!file.local_deleted&&file.completed===file.length&&['completed','paused','seeding'].includes(job.status)){const link=text('a','保存 '+file.name);link.href='/api/transfers/'+job.id+'/files/'+file.index;link.download=file.name;actions.append(link);}}
async function appendTransferFiles(){const files=await api('/transfers/files');for(const file of files){const row=text('article','','account-row'),info=text('div');info.append(text('strong',file.name),text('p',bytes(file.size)+(file.seeding?' · 正在做种':'')));const link=text('a','保存到设备');link.href=file.url;link.download=file.name;row.append(info,link);$('local-files').append(row);}}

function batchUuid(){const b=crypto.getRandomValues(new Uint8Array(16));b[6]=(b[6]&15)|64;b[8]=(b[8]&63)|128;const h=[...b].map(v=>v.toString(16).padStart(2,'0')).join('');return [h.slice(0,8),h.slice(8,12),h.slice(12,16),h.slice(16,20),h.slice(20)].join('-');}

let githubCurrent=null,githubReleases=[],githubPage=1,githubRevision=0;
function githubFiles(){return githubReleases[Number($('github-release').value)]?.files||[];}
function updateGithubSelection(){
  const indices=[...$('github-assets').querySelectorAll('input:checked')].map(n=>Number(n.value)),files=indices.map(i=>githubFiles()[i]),known=files.reduce((n,f)=>n+(f.size||0),0);
  $('github-selection').textContent='已选 '+files.length+' 项 · '+bytes(known)+(files.some(f=>f.size==null)?' + 源码 ZIP 大小待下载时确定':'');$('github-download').disabled=!files.length;
}
function renderGithubFiles(){
  const release=githubReleases[Number($('github-release').value)];$('github-assets').replaceChildren();
  $('github-release-info').textContent=release?release.tag+(release.prerelease?' · 预发布':'')+(release.published_at?' · '+release.published_at.slice(0,10):''):'';
  for(const [i,file]of githubFiles().entries()){
    const row=text('label','','extracted-link'),check=document.createElement('input');check.type='checkbox';check.value=i;check.onchange=updateGithubSelection;
    const info=text('div');info.append(text('strong',file.name),text('small',file.source?'此 Release 的源码 ZIP · 大小待下载时确定':bytes(file.size||0)));row.append(check,info);$('github-assets').append(row);
  }
  updateGithubSelection();
}
async function openGithub(raw,more=false){
  const repo=githubRepository(new URL(raw));if(!repo)throw Error('请粘贴公开 GitHub 仓库或 Release 页面地址');
  const revision=++githubRevision;
  if(!more){githubCurrent={...repo,inspectUrl:raw};githubReleases=[];githubPage=1;$('github-repository').textContent=repo.owner+'/'+repo.repo;renderGithubFiles();$('github-release').replaceChildren();}
  $('github-panel').hidden=false;$('github-more').disabled=true;$('github-note').textContent='正在读取 Release，或直接点击“下载仓库 ZIP”…';
  $('github-all-releases').hidden=!/\/releases\/(?:tag|latest)(?:\/|$)/.test(new URL(raw).pathname);
  try{
    const result=await api('/github/inspect',{method:'POST',body:JSON.stringify({url:raw,page:more?githubPage+1:1})});
    if(revision!==githubRevision)return;
    const previous=$('github-release').value;githubReleases=more?[...githubReleases,...result.releases]:result.releases;githubPage=result.page;
    $('github-release').replaceChildren();for(const [i,release]of githubReleases.entries()){const option=text('option',release.tag+' · '+release.name+(release.prerelease?' · 预发布':''));option.value=i;$('github-release').append(option);}
    if(more)$('github-release').value=previous;
    $('github-more').disabled=!result.has_more;$('github-note').textContent=githubReleases.length?'选择版本并勾选附件；仓库 ZIP 独立下载默认分支，Release 中的源码 ZIP 对应该版本。':'没有已发布的 Release，仍可下载仓库 ZIP。';renderGithubFiles();
  }catch(e){if(revision===githubRevision)$('github-note').textContent=e.message+'。可重试解析，或直接下载仓库 ZIP。';}
}
async function downloadGithub(files,groupName,button){
  if(!files.length)return;if(files.length>100)throw Error('每批最多选择 100 个附件');
  const group_id=batchUuid(),headers=headersInput();let added=0;button.disabled=true;
  try{
    for(const file of files){await api('/transfers',{method:'POST',body:JSON.stringify({uri:file.url,name:file.name,headers,group_id,group_name:$('link-batch').value||groupName,create_folder:$('link-create-folder').checked})});added++;}
    await loadJobs();setView('tasks');toast('已添加 '+added+' 个 GitHub 下载');
  }catch(e){throw Error((added?'已添加 '+added+' 项；':'')+e.message);}finally{button.disabled=false;}
}
$('github-release').onchange=renderGithubFiles;
$('github-more').onclick=()=>openGithub(githubCurrent.inspectUrl,true).catch(e=>toast(e.message));
$('github-all-releases').onclick=()=>openGithub(githubCurrent.url).catch(e=>toast(e.message));
$('github-close').onclick=()=>{githubRevision++;$('github-panel').hidden=true;};
$('github-select-all').onclick=()=>{for(const n of $('github-assets').querySelectorAll('input'))n.checked=true;updateGithubSelection();};
$('github-select-none').onclick=()=>{for(const n of $('github-assets').querySelectorAll('input'))n.checked=false;updateGithubSelection();};
$('github-zip').onclick=async()=>{if(!githubCurrent)return;try{const {owner,repo}=githubCurrent;await downloadGithub([{url:'https://github.com/'+owner+'/'+repo+'/archive/HEAD.zip',name:repo+'.zip'}],owner+'/'+repo+' · 仓库源码',$('github-zip'));}catch(e){toast(e.message);}};
$('github-download').onclick=async()=>{const files=[...$('github-assets').querySelectorAll('input:checked')].map(n=>githubFiles()[Number(n.value)]),release=githubReleases[Number($('github-release').value)];try{await downloadGithub(files,$('github-repository').textContent+' · '+release.tag,$('github-download'));}catch(e){toast(e.message);}finally{updateGithubSelection();}};

let torrentCurrent=null;
function openTorrent(job){torrentCurrent=job;$('torrent-files').replaceChildren();for(const f of job.files){const row=text('label','','torrent-row'),check=document.createElement('input');check.type='checkbox';check.value=f.index;check.checked=f.selected;check.onchange=updateTorrentSelection;row.append(check,text('span',f.name),text('small',bytes(f.length)));$('torrent-files').append(row);}updateTorrentSelection();$('torrent-dialog').showModal();}
function updateTorrentSelection(){const indices=[...$('torrent-files').querySelectorAll('input:checked')].map(n=>Number(n.value));const files=(torrentCurrent?.files||[]).filter(f=>indices.includes(f.index));$('torrent-summary').textContent=`已选 ${files.length} / ${torrentCurrent?.files.length||0} 个文件 · ${bytes(files.reduce((n,f)=>n+f.length,0))}`;$('torrent-start').disabled=!files.length;}
$('torrent-all').onclick=()=>{for(const n of $('torrent-files').querySelectorAll('input'))n.checked=true;updateTorrentSelection();};$('torrent-none').onclick=()=>{for(const n of $('torrent-files').querySelectorAll('input'))n.checked=false;updateTorrentSelection();};$('close-torrent').onclick=()=>$('torrent-dialog').close();
$('torrent-start').onclick=async()=>{try{const indices=[...$('torrent-files').querySelectorAll('input:checked')].map(n=>Number(n.value));await api('/transfers/'+torrentCurrent.id+'/select',{method:'POST',body:JSON.stringify({indices})});$('torrent-dialog').close();await loadJobs();}catch(e){toast(e.message);}};
async function addTorrentFiles(files){
  const list=[...files];if(!list.length)return;if(list.length>20)throw Error('一次最多添加 20 个种子文件');
  if(list.some(file=>!file.name.toLowerCase().endsWith('.torrent')||file.size>8*1024*1024))throw Error('请选择不超过 8 MiB 的 .torrent 文件');
  const group_id=batchUuid();let added=0;
  try{for(const file of list){const data=new Uint8Array(await file.arrayBuffer());let raw='';for(let i=0;i<data.length;i+=16384)raw+=String.fromCharCode(...data.subarray(i,i+16384));await api('/transfers',{method:'POST',body:JSON.stringify({torrent_base64:btoa(raw),name:file.name,group_id,group_name:list.length>1?'种子批次':'种子 · '+file.name,create_folder:$('link-create-folder').checked})});added++;}}
  catch(e){throw Error((added?'已添加 '+added+' 个种子；':'')+e.message);}
  setView('tasks');await loadJobs();toast('已添加 '+added+' 个种子，文件列表准备好后请点击“选择文件”');
}
$('choose-torrent').onclick=()=>$('torrent-file').click();
$('torrent-file').onchange=async()=>{try{await addTorrentFiles($('torrent-file').files);}catch(e){toast(e.message);}finally{$('torrent-file').value='';}};
let torrentDragDepth=0;
document.addEventListener('dragenter',event=>{if(!event.dataTransfer?.types.includes('Files'))return;event.preventDefault();torrentDragDepth++;$('torrent-drop-zone').hidden=false;});
document.addEventListener('dragover',event=>{if(!event.dataTransfer?.types.includes('Files'))return;event.preventDefault();event.dataTransfer.dropEffect='copy';});
document.addEventListener('dragleave',event=>{if(!event.dataTransfer?.types.includes('Files'))return;if(--torrentDragDepth<=0){torrentDragDepth=0;$('torrent-drop-zone').hidden=true;}});
document.addEventListener('drop',async event=>{if(!event.dataTransfer?.files.length)return;event.preventDefault();torrentDragDepth=0;$('torrent-drop-zone').hidden=true;try{await addTorrentFiles(event.dataTransfer.files);}catch(e){toast(e.message);}});
window.addEventListener('blur',()=>{torrentDragDepth=0;$('torrent-drop-zone').hidden=true;});

let extractedLinks=[];
$('inspect-page').onclick=async()=>{const url=$('share-url').value.trim();if(url.split('\n').length!==1){toast('每次解析一个网页地址');return;}const b=$('inspect-page');b.disabled=true;try{const result=await api('/links/inspect',{method:'POST',body:JSON.stringify({url})});extractedLinks=result.links;$('page-link-list').replaceChildren();$('page-links-note').textContent=result.message||(result.direct?'这是一个直接文件链接'+(result.size!=null?' · '+bytes(result.size):''):'请选择需要的文件链接');for(const [i,item]of extractedLinks.entries()){const row=text('label','','extracted-link'),check=document.createElement('input');check.type='checkbox';check.value=i;const label=text('div');label.append(text('strong',item.name||item.url),text('small',item.url));row.append(check,label);$('page-link-list').append(row);}$('page-links').hidden=false;if(!extractedLinks.length)$('page-links-note').textContent='未找到静态链接。需要运行脚本或登录的网页，请取得实际文件链接后粘贴。';}catch(e){toast(e.message);}finally{b.disabled=false;}};
$('page-links-download').onclick=()=>{const selected=[...$('page-link-list').querySelectorAll('input:checked')].map(n=>extractedLinks[Number(n.value)].url);if(!selected.length){toast('请先选择链接');return;}$('share-url').value=selected.join('\n');$('share-form').requestSubmit();};

$('bandwidth-form').onsubmit=async event=>{event.preventDefault();try{const body={download_limit:Number($('download-limit').value)*1024,upload_limit:Number($('upload-limit').value)*1024,seed_ratio:Number($('seed-ratio').value),seed_minutes:Number($('seed-minutes').value)};await api('/settings',{method:'PUT',body:JSON.stringify(body)});toast('限速与做种策略已保存');}catch(e){toast(e.message);}};

let statisticsBusy=false;
async function loadStatistics(){if(statisticsBusy)return;statisticsBusy=true;try{const s=await api('/statistics');$('statistics-cards').replaceChildren();for(const [title,value,note]of [['当前下载',bytes(s.download_speed)+'/s','全引擎文件传输'],['当前做种上传',bytes(s.upload_speed)+'/s','BitTorrent 数据'],['累计接收',bytes(s.received_bytes),'保留的旧任务与新传输'],['累计做种上传',bytes(s.uploaded_bytes),'启用统计后的记录'],['当前任务已下载',bytes(s.completed_bytes),'/ '+bytes(s.planned_bytes)],['下载空间可用',s.disk?bytes(s.disk.free):'暂未获取',s.disk?'总容量 '+bytes(s.disk.total):''],['下载峰值',bytes(s.peak_download_speed)+'/s','统计记录中的峰值'],['上传峰值',bytes(s.peak_upload_speed)+'/s','统计记录中的峰值']]){const card=text('article','','statistic-card');card.append(text('small',title),text('strong',value),text('p',note));$('statistics-cards').append(card);}
  $('statistics-download').replaceChildren(speedChart({speed_history:s.history,active_ms:1}));$('statistics-upload').replaceChildren(speedChart({speed_history:s.history.map(p=>({at_ms:p.at_ms,speed:p.upload_speed})),active_ms:1}));
  for(const [id,values]of [['statistics-counts',s.counts],['statistics-sources',s.sources]]){$(id).replaceChildren();for(const [key,n]of Object.entries(values)){const row=text('div','','statistics-row');row.append(text('span',labels[key]||({http:'HTTP',https:'HTTPS',ftp:'FTP',sftp:'SFTP',magnet:'磁力',torrent:'BT',baidu:'百度',quark:'夸克',thunder:'迅雷',google:'Google Drive',onedrive:'OneDrive'}[key])||key),text('b',n+' 项'));$(id).append(row);}}
  $('statistics-daily').replaceChildren();for(const day of [...s.daily].reverse()){const row=text('tr');row.append(text('td',new Date(day.day*86400000).toISOString().slice(0,10)),text('td',bytes(day.received)),text('td',bytes(day.sent)));$('statistics-daily').append(row);}$('statistics-note').textContent=s.totals_since+'。累计量不包括协议开销；BT 以引擎确认的文件数据计数，网盘重传量计入接收统计。';
 }finally{statisticsBusy=false;}}
$('statistics-settings').onclick=()=>setView('browse');
