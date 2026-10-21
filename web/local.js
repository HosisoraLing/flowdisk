'use strict';
let localPath='/',localEntries=[],localPicked=new Set(),localMovePaths=[],localDestination='/';
function localParent(path){return path.slice(0,path.lastIndexOf('/'))||'/';}
function localWithin(parent,path){return path===parent||path.startsWith(parent.replace(/\/$/,'')+'/');}
function localBreadcrumbs(container,path,visit){
  container.replaceChildren(button('下载目录',()=>visit('/')));
  let current='';for(const part of path.split('/').filter(Boolean)){current+='/'+part;const target=current;container.append(text('span',' / '),button(part,()=>visit(target)));}
}
function updateLocalSelection(){const n=localPicked.size;$('local-count').textContent=n||'';$('local-move').disabled=!n;}
async function localOperation(body){const result=await api('/local/operation',{method:'POST',body:JSON.stringify(body)});await loadLocal();await loadJobs();return result;}
async function loadLocal(path=localPath){
  const result=await api('/local/tree?path='+encodeURIComponent(path));
  if(path!==localPath)localPicked.clear();localPath=result.path;localEntries=result.entries;
  localPicked=new Set([...localPicked].filter(p=>localEntries.some(e=>e.path===p&&!e.locked)));
  localBreadcrumbs($('local-breadcrumbs'),localPath,visit=>loadLocal(visit).catch(e=>toast(e.message)));
  $('local-files').replaceChildren();updateLocalSelection();
  if(!localEntries.length){$('local-files').append(text('div','这个文件夹还是空的。','empty'));return;}
  const table=text('table','','local-table'),head=text('thead'),headrow=text('tr'),checkHead=text('th'),all=document.createElement('input');all.type='checkbox';all.setAttribute('aria-label','全选当前文件夹');const selectable=localEntries.filter(e=>!e.locked);all.checked=!!selectable.length&&selectable.every(e=>localPicked.has(e.path));all.onchange=()=>{localPicked=new Set(all.checked?selectable.map(e=>e.path):[]);renderAgain();};checkHead.append(all);headrow.append(checkHead,text('th','名称'),text('th','大小'),text('th','操作'));head.append(headrow);table.append(head);const body=text('tbody');
  function renderAgain(){loadLocal().catch(e=>toast(e.message));}
  for(const file of localEntries){const row=text('tr'),checkCell=text('td'),check=document.createElement('input');check.type='checkbox';check.checked=localPicked.has(file.path);check.disabled=file.locked;check.setAttribute('aria-label','选择 '+file.name);check.onchange=()=>{check.checked?localPicked.add(file.path):localPicked.delete(file.path);updateLocalSelection();all.checked=selectable.every(e=>localPicked.has(e.path));};checkCell.append(check);
    const info=text('td'),line=text('div','','local-name');line.append(text('span',file.is_dir?'▰':'◇','kind-icon'));if(file.is_dir)line.append(button(file.name,()=>loadLocal(file.path).catch(e=>toast(e.message)),'local-folder'));else line.append(text('strong',file.name));info.append(line);if(file.locked)info.append(text('small','包含下载或做种内容，暂不可移动','local-locked'));if(file.passwords?.length)info.append(passwordNotes(file.passwords));
    const actions=text('td','','local-actions');if(!file.is_dir){const link=text('a','保存到设备');link.href=file.download_url||('/api/local/tree/file?path='+encodeURIComponent(file.path));link.download=file.name;actions.append(link);}
    const move=button('移动',()=>openLocalMove([file.path]));move.disabled=file.locked;
    const rename=button('重命名',async()=>{const name=prompt('新的名称',file.name);if(!name||name===file.name)return;try{await localOperation({action:'rename',paths:[file.path],name});toast('已重命名');}catch(e){toast(e.message);}});rename.disabled=file.locked;
    const remove=button('删除',async()=>{if(!confirm(file.is_dir?'删除空文件夹“'+file.name+'”？非空文件夹不会被删除。':'删除服务器本地文件“'+file.name+'”？云端原文件会保留。'))return;try{await localOperation({action:'delete',paths:[file.path]});toast('已删除');}catch(e){toast(e.message);}});remove.disabled=file.locked;
    actions.append(move,rename,remove);row.append(checkCell,info,text('td',file.is_dir?'文件夹':bytes(file.size)),actions);body.append(row);
  }table.append(body);$('local-files').append(table);
}
$('refresh-local').onclick=()=>loadLocal().catch(e=>toast(e.message));
$('local-mkdir').onclick=async()=>{const name=prompt('新文件夹名称');if(!name)return;try{await localOperation({action:'mkdir',path:localPath,name});toast('文件夹已创建');}catch(e){toast(e.message);}};
$('local-move').onclick=()=>openLocalMove([...localPicked]);
const moveDialog=text('dialog','','local-move-dialog');moveDialog.id='local-move-dialog';const moveHeader=text('div','','page-heading');moveHeader.append(text('h2','移动到文件夹'),button('关闭',()=>moveDialog.close()));const moveNote=text('p'),moveCrumbs=text('nav'),moveFolders=text('div','','local-move-folders'),moveFooter=text('div','','tool-actions'),moveConfirm=button('移动到此文件夹',async()=>{moveConfirm.disabled=true;try{const result=await localOperation({action:'move',paths:localMovePaths,destination:localDestination});localPicked.clear();moveDialog.close();toast('已移动 '+result.moved+' 项，未复制文件内容');}catch(e){toast(e.message);await loadLocal().catch(()=>{});}finally{moveConfirm.disabled=false;}},'primary');moveCrumbs.setAttribute('aria-label','移动目标目录');moveFooter.append(moveConfirm);moveDialog.append(moveHeader,moveNote,moveCrumbs,moveFolders,moveFooter);document.body.append(moveDialog);
async function openLocalMove(paths){localMovePaths=paths;moveNote.textContent='已选 '+paths.length+' 项。选择目标后直接移动；同名内容不会被覆盖。';if(!moveDialog.open)moveDialog.showModal();await browseLocalDestination(localPath);}
async function browseLocalDestination(path){
  try{moveConfirm.disabled=true;const result=await api('/local/tree?path='+encodeURIComponent(path));localDestination=result.path;localBreadcrumbs(moveCrumbs,path,browseLocalDestination);moveFolders.replaceChildren();
    if(path!=='/')moveFolders.append(button('↑ 上一级',()=>browseLocalDestination(localParent(path))));
    for(const folder of result.entries.filter(e=>e.is_dir&&!localMovePaths.some(p=>localWithin(p,e.path)))){moveFolders.append(button('▰ '+folder.name,()=>browseLocalDestination(folder.path),'local-target-folder'));}
    if(!moveFolders.children.length)moveFolders.append(text('p','没有其他子文件夹，可移动到当前目录。'));
    moveConfirm.disabled=localMovePaths.some(p=>localWithin(p,path)||localParent(p)===path);moveConfirm.textContent='移动到 '+(path==='/'?'下载目录':path.split('/').pop());
  }catch(e){toast(e.message);}
}
