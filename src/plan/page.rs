const MARK: &str = include_str!("../../assets/gray-mark.svg");

pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub fn signed_in() -> String {
    render(
        "ok",
        "Connected",
        "Signed in with ChatGPT",
        "Gray can now use your ChatGPT plan. You can close this tab and return to Gray.",
    )
}

pub fn failed(message: &str) -> String {
    let message = message.trim();
    let detail = if message.contains("/connect") {
        message.to_string()
    } else {
        format!("{message} Run /connect in Gray to try again.")
    };
    render("error", "Not connected", "Sign-in didn't complete", &detail)
}

fn render(state: &str, status: &str, title: &str, detail: &str) -> String {
    let (title, detail) = (escape(title), escape(detail));
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Gray · {title}</title>
<style>
  :root {{
    --bg: #050506; --card: #0a0a0b; --line: #1c1c20; --paper: #eceae7;
    --dim: #8b8a86; --dimmer: #5d5c59; --ok: #00d294; --error: #ff6568;
    color-scheme: dark;
  }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; margin: 0; }}
  body {{
    background: var(--bg); color: var(--paper);
    font: 15px/1.55 "Plus Jakarta Sans", ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    -webkit-font-smoothing: antialiased;
    display: grid; place-items: center; padding: 24px 16px;
  }}
  main {{
    width: 100%; max-width: 420px; text-align: center;
    background: var(--card); border: 1px solid var(--line); border-radius: 16px;
    padding: 40px 32px 28px;
  }}
  .mark {{ width: 52px; height: auto; color: var(--paper); display: block; margin: 0 auto 28px; }}
  .mark svg {{ width: 100%; height: auto; display: block; }}
  .status {{
    display: inline-flex; align-items: center; gap: 8px;
    font: 12px/1 "Departure Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    letter-spacing: .04em; text-transform: uppercase; color: var(--dim);
    border: 1px solid var(--line); border-radius: 999px; padding: 6px 10px 6px 9px;
  }}
  .dot {{ width: 7px; height: 7px; border-radius: 50%; background: var(--dot); box-shadow: 0 0 10px var(--dot); }}
  .ok {{ --dot: var(--ok); }} .error {{ --dot: var(--error); }}
  h1 {{ font-size: 22px; line-height: 1.25; font-weight: 600; letter-spacing: -.01em; margin: 18px 0 8px; }}
  p {{ margin: 0; color: var(--dim); }}
  footer {{
    margin-top: 32px; padding-top: 16px; border-top: 1px solid var(--line);
    font: 12px/1 "Departure Mono", ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--dimmer);
  }}
</style>
</head>
<body>
<main class="{state}">
  <div class="mark">{MARK}</div>
  <span class="status"><span class="dot"></span>{status}</span>
  <h1>{title}</h1>
  <p>{detail}</p>
  <footer>gray.alignment.id</footer>
</main>
</body>
</html>
"#
    )
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
