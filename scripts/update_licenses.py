#!/usr/bin/env python3
"""Regenerate public license notices from resolved Linux Cargo dependencies."""
from pathlib import Path
import json,subprocess,html
root=Path(__file__).resolve().parents[1]
meta=json.loads(subprocess.check_output(['cargo','metadata','--format-version','1','--locked','--filter-platform','x86_64-unknown-linux-gnu'],cwd=root))
resolved={n['id']for n in meta['resolve']['nodes']}
rows=[];texts=[]
for p in sorted(meta['packages'],key=lambda p:p['name']):
 if p['id']not in resolved or p['name']=='flowdisk':continue
 name=p['name'];version=p['version'];license=p.get('license')or'见许可证原文';source=p.get('repository')or'https://crates.io/crates/'+name
 if not source.startswith(('https://','http://')):source='https://crates.io/crates/'+name
 rows.append({'name':name,'version':version,'license':license,'source':source})
 folder=Path(p['manifest_path']).parent
 for file in sorted(folder.iterdir()):
  if file.is_file()and file.name.upper().startswith(('LICENSE','LICENCE','COPYING','NOTICE','COPYRIGHT')):
   try:content=file.read_text()
   except(UnicodeDecodeError,OSError):continue
   texts.append('\n'+'='*72+'\n'+name+' '+version+' — '+file.name+'\n'+'='*72+'\n'+content+'\n')
(root/'THIRD_PARTY_LICENSES.txt').write_text('Third-party Rust crate copyright and license notices\nGenerated from Cargo.lock. OpenList, aria2 and browser bundles have separate notices.\n'+''.join(texts))
metadata={'rust':rows,'browser':[{'name':'marked','version':'18.1.1','license':'MIT','source':'https://github.com/markedjs/marked','notice':'/vendor/marked-LICENSE.txt'},{'name':'DOMPurify','version':'3.4.17','license':'MPL-2.0 OR Apache-2.0','source':'https://github.com/cure53/DOMPurify','notice':'/vendor/purify-LICENSE.txt'}]}
(root/'web/vendor/notices.json').write_text(json.dumps(metadata,ensure_ascii=False,indent=2)+'\n')
def row(p):return '<tr><td>'+html.escape(p['name'])+'</td><td>'+html.escape(p['version'])+'</td><td>'+html.escape(p['license'])+'</td><td><a href="'+html.escape(p['source'],quote=True)+'" rel="noopener noreferrer">项目源码</a>'+(' · <a href="'+p['notice']+'">许可原文</a>'if'notice'in p else'')+'</td></tr>'
def table(items):return '<table><thead><tr><th>组件</th><th>版本</th><th>许可证</th><th>来源</th></tr></thead><tbody>'+''.join(map(row,items))+'</tbody></table>'
page='''<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>开源软件使用声明 · FlowDisk</title><link rel="stylesheet" href="/style.css"></head><body class="legal-page"><main><a class="legal-back" href="/">← 返回工作台</a><div class="eyebrow">FLOWDISK / OPEN SOURCE NOTICES</div><h1>开源软件使用声明</h1><p>FlowDisk 使用下列开源组件。感谢其作者与维护者；各组件的版权和许可证归原作者所有，本页不取代许可原文。</p><h2>FlowDisk 原创代码</h2><p>采用 <a href="/license.txt">MIT 许可证</a>，版权归 FlowDisk contributors。<a href="https://github.com/HosisoraLing/flowdisk">公开源码</a>包含应用、部署文件和测试。</p><h2>独立服务与下载引擎</h2><p>这些组件作为独立进程分发或运行，不链接到 Rust 应用中；其各自许可证继续适用。</p>'''
page+=table([{'name':'OpenList','version':'4.2.6','license':'AGPL-3.0','source':'https://github.com/OpenListTeam/OpenList/tree/v4.2.6'},{'name':'aria2 (Debian bookworm)','version':'1.36.0-1','license':'GPL-2.0-or-later','source':'https://packages.debian.org/bookworm/aria2'}])
page+='''<p>OpenList 许可证见其源码的 LICENSE；aria2 及系统包许可见运行镜像中的 /usr/share/doc 对应组件目录。源码与软件包可从上方链接获取；重新分发修改版应按各许可证保留声明和提供相应源码。</p><h2>浏览器组件</h2>'''+table(metadata['browser'])
page+='''<p>Markdown 内容经 DOMPurify 允许列表过滤。浏览器组件随本项目本地提供，不依赖运行时 CDN。</p><h2>Rust 依赖</h2><p>以下项目根据锁定依赖生成。完整版权与许可文本见 <a href="/third-party-licenses.txt">第三方许可证汇编</a>，包括传递依赖。</p><details><summary>查看 '''+str(len(rows))+' 个 Rust 依赖组件</summary>'+table(rows)+'''</details><p>本页由仓库 scripts/update_licenses.py 生成；依赖变更后应重新生成并复核声明。</p></main></body></html>'''
(root/'web/licenses.html').write_text(page)
print('Generated declarations for',len(rows),'Rust dependencies and',len(metadata['browser']),'browser components.')
