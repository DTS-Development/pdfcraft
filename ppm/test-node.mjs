// 포크 Node 모듈 스모크 테스트: node ppm/test-node.mjs [산출 폴더]  (기본 dist/pdf-node, 먼저 ppm/build-node.sh)
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import zlib from 'node:zlib';

const here = path.dirname(fileURLToPath(import.meta.url));
const dir = process.argv[2] ?? path.join(here, '..', 'dist', 'pdf-node');
const m = createRequire(import.meta.url)(path.join(dir, 'ppm_annot_node.js'));

// NM 없는 주석만 있는 한 페이지 PDF: [0] Link, [1] Text(답글 대상), [2] Text 답글, [3] Square, [4] Highlight(한글 내용)
function samplePdf() {
  const objs = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Annots [4 0 R 5 0 R 6 0 R 7 0 R 8 0 R] >>',
    '<< /Type /Annot /Subtype /Link /Rect [10 10 50 30] /Border [0 0 0] >>',
    '<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (note) /T (Alice) /Name /Comment /F 4 /C [1 1 0] >>',
    '<< /Type /Annot /Subtype /Text /Rect [100 700 120 720] /Contents (reply) /T (Bob) /IRT 5 0 R /RT /R /F 4 /Name /Comment >>',
    '<< /Type /Annot /Subtype /Square /Rect [200 200 300 260] /C [1 0 0] /T (Carol) /F 4 /BS << /W 2 >> >>',
    '<< /Type /Annot /Subtype /Highlight /Rect [50 500 200 520] /QuadPoints [50 520 200 520 50 500 200 500] /C [1 1 0] /T (Dave) /F 4 >>',
  ];
  let out = '%PDF-1.7\n';
  const offs = [];
  objs.forEach((o, i) => { offs.push(out.length); out += `${i + 1} 0 obj\n${o}\nendobj\n`; });
  const xref = out.length;
  out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n` + offs.map((o) => `${String(o).padStart(10, '0')} 00000 n \n`).join('');
  out += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return new Uint8Array(Buffer.from(out, 'latin1'));
}

// 파일 안 모든 스트림을 풀어 한 문자열로(객체 스트림 속 글꼴 사전도 보이게)
function allText(pdf) {
  const raw = Buffer.from(pdf).toString('latin1');
  let all = raw;
  for (const mt of raw.matchAll(/stream\r?\n/g)) {
    const start = mt.index + mt[0].length;
    const end = raw.indexOf('endstream', start);
    try { all += '\n' + zlib.inflateSync(Buffer.from(raw.slice(start, end), 'latin1')).toString('latin1'); } catch { /* 압축 아님 */ }
  }
  return all;
}
const largestStream = (pdf) => {
  const raw = Buffer.from(pdf).toString('latin1');
  let max = 0;
  for (const mt of raw.matchAll(/stream\r?\n/g)) max = Math.max(max, raw.indexOf('endstream', mt.index) - mt.index);
  return max;
};

const pdf = samplePdf();

// 1. list
const list = JSON.parse(m.list(pdf));
assert.deepEqual(list.map((e) => [e.page, e.index, e.subtype]), [[1, 0, 'Link'], [1, 1, 'Text'], [1, 2, 'Text'], [1, 3, 'Square'], [1, 4, 'Highlight']]);
assert.deepEqual(list.map((e) => e.exportable), [false, true, true, true, true]);
assert.ok(list.every((e) => e.nm === null));
assert.deepEqual(list[2].irt, { page: 1, index: 1 });
assert.equal(list[3].title, 'Carol');

// 2. export: 주어진 이름·부모 이름, 원본은 그대로
const reqs = [
  { page: 1, index: 1, name: 'ppm-file-a' },
  { page: 1, index: 2, name: 'ppm-file-b', inReplyTo: 'ppm-file-a' },
  { page: 1, index: 3, name: 'ppm-file-c' },
  { page: 1, index: 0, name: 'ppm-file-link' },
  { page: 1, index: 9, name: 'ppm-file-none' },
];
const exported = JSON.parse(m.exportXfdf(pdf, JSON.stringify(reqs)));
assert.match(exported[0].xfdf, /name="ppm-file-a"/);
assert.match(exported[1].xfdf, /name="ppm-file-b"/);
assert.match(exported[1].xfdf, /inreplyto="ppm-file-a"/);
assert.match(exported[2].xfdf, /<square /);
assert.ok(exported[3].error && exported[4].error, 'Link와 없는 위치는 error');
assert.equal(Buffer.from(pdf).toString('latin1'), Buffer.from(samplePdf()).toString('latin1'), '입력 바이트 불변');

// 3. strip: 지정한 것만 지우고 나머지 순서 유지, 없는 위치는 throw
const stripped = m.strip(pdf, JSON.stringify([{ page: 1, index: 3 }, { page: 1, index: 4 }]));
assert.deepEqual(JSON.parse(m.list(stripped)).map((e) => e.subtype), ['Link', 'Text', 'Text']);
assert.throws(() => m.strip(pdf, JSON.stringify([{ page: 1, index: 42 }])), /index 42/);

// 4. 왕복: list -> export -> strip -> merge 하면 주석이 같은 수(Link 제외 모두 이름이 붙어서)로 돌아온다
const toMove = list.filter((e) => e.exportable);
const names = toMove.map((e) => `ppm-file-${e.index}`);
const out = JSON.parse(m.exportXfdf(pdf, JSON.stringify(toMove.map((e, i) => ({
  page: e.page, index: e.index, name: names[i], inReplyTo: e.irt ? `ppm-file-${e.irt.index}` : undefined,
})))));
assert.ok(out.every((o) => o.xfdf), JSON.stringify(out));
const clean = m.strip(pdf, JSON.stringify(toMove.map((e) => ({ page: e.page, index: e.index }))));
assert.deepEqual(JSON.parse(m.list(clean)).map((e) => e.subtype), ['Link']);
const back = m.merge(clean, JSON.stringify(out.map((o) => ({ annotId: o.name, authorId: 'ppm:file', xfdf: o.xfdf }))));
const again = JSON.parse(m.list(back));
assert.deepEqual(again.filter((e) => e.nm).map((e) => e.nm).sort(), [...names].sort());
assert.deepEqual(again.filter((e) => e.nm && e.irt).map((e) => e.nm), ['ppm-file-2']);

// 5. 한글 FreeText merge -> 한글 서브셋 글꼴이 들어간다
const korean = `<?xml version="1.0" encoding="UTF-8" ?><xfdf xmlns="http://ns.adobe.com/xfdf/" xml:space="preserve"><fields /><annots><freetext page="0" name="k1" title="홍길동" rect="100,400,320,460" flags="print" color="#FFFFFF"><contents>안녕하세요 한글 주석입니다</contents><defaultappearance>/Helv 14 Tf 0 0 0 rg</defaultappearance></freetext></annots></xfdf>`;
const merged = m.merge(pdf, JSON.stringify([{ annotId: 'k1', authorId: 'u-1', xfdf: korean }]));
const text = allText(merged);
assert.match(text, /\+NanumGothic/, '한글 서브셋 글꼴 이름');
assert.match(text, /\/FontFile2/);
assert.ok(largestStream(merged) < 600_000, `글꼴 전체(약 2MB)가 아니라 서브셋만: ${largestStream(merged)}`);
const k = JSON.parse(m.list(merged)).find((e) => e.nm === 'k1');
assert.ok(k && k.subtype === 'FreeText' && k.title === '홍길동' && k.exportable);
assert.throws(() => m.merge(pdf, JSON.stringify([{ annotId: 'bad', authorId: 'u', xfdf: '<nope/>' }])), /bad/);

// 6. 평탄화: 주석(Square 등)은 페이지 내용이 되어 목록에서 빠지고 링크는 남는다
const flat = JSON.parse(m.list(m.flatten(pdf))).map((e) => e.subtype);
assert.deepEqual(flat, ['Link'], '주석은 굽고 링크만 남는다');
assert.throws(() => m.flatten(new Uint8Array([1, 2, 3])), /can't be opened/);

// 7. 텍스트·좌표 추출: 400x200 쪽 (50,150)에 글자. 보이는 쪽 기준(왼쪽 위 원점, y 아래), 쪽 번호 1부터
function textPdf(extra) {
  const content = 'BT /F1 20 Tf 50 150 Td (Hello world) Tj ET';
  const objs = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 200] ${extra} /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>`,
    `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let out = '%PDF-1.7\n';
  const offs = [];
  objs.forEach((o, i) => { offs.push(out.length); out += `${i + 1} 0 obj\n${o}\nendobj\n`; });
  const xref = out.length;
  out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n` + offs.map((o) => `${String(o).padStart(10, '0')} 00000 n \n`).join('');
  out += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return new Uint8Array(Buffer.from(out, 'latin1'));
}
const t0 = JSON.parse(m.extractText(textPdf(''), '{"granularity":"word"}'))[0];
assert.deepEqual([t0.page, t0.width, t0.height, t0.rotation], [1, 400, 200, 0]);
assert.equal(t0.lines[0].text, 'Hello world');
assert.deepEqual(t0.lines[0].words.map((w) => w.text), ['Hello', 'world']);
assert.ok(Math.abs(t0.lines[0].bbox[0] - 50) < 2 && t0.lines[0].bbox[3] > 45 && t0.lines[0].bbox[3] < 55, '왼쪽 위 원점, y 아래');
const t90 = JSON.parse(m.extractText(textPdf('/Rotate 90'), ''))[0];
assert.deepEqual([t90.width, t90.height, t90.rotation], [200, 400, 90]);
assert.throws(() => m.extractText(textPdf(''), '{"pages":[2]}'), /page 2 does not exist/);
assert.throws(() => m.extractText(textPdf(''), '{"pages":[0]}'), /numbered from 1/);
assert.throws(() => m.extractText(new Uint8Array([1, 2, 3]), ''), /can't be opened/);

console.log('ok: list/exportXfdf/strip/merge/flatten/extractText, 한글 서브셋 글꼴', largestStream(merged), 'bytes max stream');
