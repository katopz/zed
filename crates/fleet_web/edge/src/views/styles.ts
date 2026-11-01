export const STYLES = `
:root{--bg:#1e2025;--panel:#25272d;--panel2:#2c2f36;--border:#363a42;--text:#d6d9de;--dim:#8b919c;--accent:#74ade8;--ok:#a1c181;--warn:#dec184;--err:#e06c75;--user:#2f3440;
--mono:ui-monospace,"SF Mono",Menlo,Consolas,"Liberation Mono",monospace;--sans:system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
*{box-sizing:border-box}
html,body{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 var(--sans)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.mono,code,pre{font-family:var(--mono);font-size:12.5px}
.dim{color:var(--dim)}.warn{color:var(--warn)}
.topbar{display:flex;align-items:center;gap:12px;padding:10px 20px;border-bottom:1px solid var(--border);background:var(--panel);position:sticky;top:0;z-index:2}
.topbar .brand{font-weight:600;color:var(--text)}.topbar .spacer{flex:1}
.page{max-width:1200px;margin:0 auto;padding:16px 20px 48px}
.section-head{display:flex;align-items:baseline;gap:12px;margin:20px 0 10px}
h2{font-size:16px;margin:0}h3{font-size:14px;margin:0}h4{font-size:11px;text-transform:uppercase;letter-spacing:.05em;color:var(--dim);margin:10px 0 4px}
.banner{background:#3b2a2d;border:1px solid var(--err);color:#f2c4c8;padding:8px 12px;border-radius:6px;margin:8px 0}
.card{background:var(--panel);border:1px solid var(--border);border-radius:8px;padding:12px 14px}
.rig-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(320px,1fr));gap:12px}
.rig-head{display:flex;align-items:center;gap:8px}.rig-head .badge{margin-left:auto}
.badge{font-size:11px;font-weight:600;padding:1px 8px;border-radius:10px;border:1px solid}
.badge-quiet{color:var(--ok);border-color:var(--ok)}.badge-busy{color:var(--warn);border-color:var(--warn)}.badge-offline{color:var(--err);border-color:var(--err)}
.rig-offline{opacity:.75}
.stats{display:flex;flex-wrap:wrap;gap:6px 16px;margin:10px 0 0}.stats div{display:flex;gap:6px}.stats dt{color:var(--dim)}.stats dd{margin:0}
.reasons{margin:6px 0 0;padding-left:18px;color:var(--warn);font-size:12.5px}
.jobs,.agents,.thread-list{list-style:none;margin:0;padding:0}
.job{display:flex;flex-wrap:wrap;gap:6px;align-items:baseline;padding:2px 0}.job-cmd{word-break:break-all}
.agent{padding:4px 0;border-top:1px solid var(--border)}.agent:first-child{border-top:0}
.agent-head{display:flex;flex-wrap:wrap;gap:6px;align-items:center}.agent-name{font-weight:500}
.agent-last{color:var(--dim);font-size:12.5px;white-space:pre-wrap;overflow-wrap:anywhere;margin-top:2px}
.tag{font-size:11px;padding:0 6px;border-radius:4px;background:var(--panel2);border:1px solid var(--border);color:var(--dim);white-space:nowrap}
.tag.accent{color:var(--accent);border-color:var(--accent)}
.status-running,.tool-completed{color:var(--ok)}.status-idle{color:var(--dim)}.tool-failed,.tool-error,.status-error{color:var(--err)}
#thread-filter{margin-left:auto;background:var(--panel);border:1px solid var(--border);color:var(--text);border-radius:6px;padding:5px 10px;min-width:220px;font:inherit}
.thread-group{margin-top:14px}.thread-group h3{margin-bottom:4px}
.thread-row a{display:flex;align-items:center;gap:10px;padding:6px 10px;border-radius:6px;color:var(--text)}
.thread-row a:hover{background:var(--panel);text-decoration:none}
.thread-title{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.thread-meta{display:flex;gap:8px;align-items:center;flex-shrink:0}
.dot{width:8px;height:8px;border-radius:50%;background:var(--border);flex-shrink:0;display:inline-block}.dot.live{background:var(--ok);box-shadow:0 0 6px var(--ok)}
.thread-header{border-bottom:1px solid var(--border);padding-bottom:12px}.thread-header h1{font-size:18px;margin:6px 0}
.thread-facts{display:flex;flex-wrap:wrap;gap:8px;align-items:center}
.conversation{display:flex;flex-direction:column;gap:12px;padding-top:16px;max-width:860px}
.entry.user{align-self:flex-end;max-width:85%;background:var(--user);border:1px solid var(--border);border-radius:10px;padding:8px 12px}
.entry.agent{padding:0 2px}
.block{margin:6px 0}
details.block{background:var(--panel);border:1px solid var(--border);border-radius:6px}
details.block summary{cursor:pointer;padding:4px 10px;display:flex;gap:8px;align-items:center;color:var(--dim)}
details.block[open] summary{border-bottom:1px solid var(--border)}
details.block>pre,details.block>.markdown{margin:0;padding:8px 10px}
details.thinking .markdown{color:var(--dim)}
pre{white-space:pre-wrap;overflow-wrap:anywhere;max-height:480px;overflow:auto}
.markdown p{margin:6px 0}.markdown pre{background:var(--panel);border:1px solid var(--border);border-radius:6px;padding:8px 10px}
.markdown code{background:var(--panel2);padding:0 4px;border-radius:3px}.markdown pre code{background:none;padding:0}
.markdown table{border-collapse:collapse}.markdown td,.markdown th{border:1px solid var(--border);padding:2px 8px}
.markdown blockquote{margin:6px 0;padding-left:10px;border-left:3px solid var(--border);color:var(--dim)}
.mention{color:var(--accent)}
.resume{display:flex;align-items:center;gap:10px;color:var(--dim);font-size:12px}.resume::before,.resume::after{content:"";flex:1;border-top:1px dashed var(--border)}
.center{max-width:420px;margin:15vh auto;text-align:center}
@media (max-width:640px){.page{padding:12px}.thread-meta .mono{display:none}#thread-filter{min-width:0;width:100%}.section-head{flex-wrap:wrap}.entry.user{max-width:100%}}
`;
