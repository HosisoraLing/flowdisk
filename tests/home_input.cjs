// Run: node tests/home_input.cjs — no dependencies or network.
const assert=require('node:assert/strict'),fs=require('node:fs'),vm=require('node:vm'),path=require('node:path');
const source=fs.readFileSync(path.join(__dirname,'../web/tools.js'),'utf8');
const start=source.indexOf('function downloadInputs('),end=source.indexOf('async function submitDownloadInput(',start);
const ctx={URL};vm.createContext(ctx);vm.runInContext(source.slice(start,end),ctx);
const parse=raw=>JSON.parse(JSON.stringify(ctx.downloadInputs(raw)));
assert.deepEqual(parse('https://example.com/a.zip\nmagnet:?xt=urn:btih:0123456789012345678901234567890123456789').map(x=>x.share),[false,false]);
const share='分享：https://pan.quark.cn/s/fixture 提取码：abcd';assert.equal(parse(share)[0].url,share);assert.equal(parse(share)[0].share,true);
for(const url of ['https://drive.google.com/drive/folders/fixture','https://1drv.ms/f/fixture','https://tenant.sharepoint.com/:f:/fixture'])assert.equal(parse(url)[0].share,true);
for(const url of ['https://pan.quark.cn.evil.example/s/fixture','ftp://example.com/file','sftp://example.com/file','https://example.sharepoint.com.evil.example/:f:/fixture'])assert.equal(parse(url)[0].share,false);
for(const suffix of ['', '/', '.git', '/releases', '/releases/latest', '/releases/tag/release%2F1'])assert.deepEqual(parse('https://github.com/owner/repo'+suffix)[0].github,{owner:'owner',repo:'repo',url:'https://github.com/owner/repo'});
for(const url of ['https://github.com.evil.example/o/r','https://user@github.com/o/r','https://github.com:444/o/r','https://github.com/o/r/releases/download/v1/app.zip','https://raw.githubusercontent.com/o/r/main/a.txt','https://github.com/o/r/issues'])assert.equal(parse(url)[0].github,null);
assert.throws(()=>parse('javascript:alert(1)'));assert.throws(()=>parse(Array(101).fill('https://example.com/f').join('\n')));
console.log('PASS shared download input routing, GitHub repository/Release detection, direct asset links and URL boundaries');
