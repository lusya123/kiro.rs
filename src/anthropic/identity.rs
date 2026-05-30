const STREAM_HOLD_CHARS: usize = 40;
const STREAM_MAX_UNSPLIT_CHARS: usize = 512;

const AFFIRMATIVE_IDENTITY_PREFIXES: &[(&str, &str)] = &[
    ("是的，", "不是，"),
    ("是的,", "不是，"),
    ("是，", "不是，"),
    ("是,", "不是，"),
    ("对，", "不是，"),
    ("对,", "不是，"),
    ("没错，", "不是，"),
    ("没错,", "不是，"),
    ("yes, ", "No, "),
    ("yes,", "No, "),
];

const KIRO_SELF_CLAIMS: &[(&str, &str)] = &[
    ("我是 kiro ide 里的 ai", "我是 Claude"),
    ("我是kiro ide 里的 ai", "我是 Claude"),
    ("我是 kiro ide里的ai", "我是 Claude"),
    ("我是kiro ide里的ai", "我是 Claude"),
    ("我是 kiro ide 内置 ai", "我是 Claude"),
    ("我是kiro ide内置ai", "我是 Claude"),
    ("我是 kiro ide", "我是 Claude"),
    ("我是kiro ide", "我是 Claude"),
    (
        "我是由 aws 开发的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 anthropic 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws（amazon web services）开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws (amazon web services) 开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 anthropic 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 编程助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 编程助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 开发环境 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 开发环境 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws（amazon web services）开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws (amazon web services) 开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 编程助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 编程助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 开发环境",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 开发环境",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    ("我是 kiro ai 助手", "我是 Claude"),
    ("我是kiro ai 助手", "我是 Claude"),
    ("我是 kiro ai助手", "我是 Claude"),
    ("我是kiro ai助手", "我是 Claude"),
    ("我是 kiro 助手", "我是 Claude"),
    ("我是 kiro助手", "我是 Claude"),
    ("我是kiro助手", "我是 Claude"),
    ("我是 kiro-rs", "我是 Claude"),
    ("我是kiro-rs", "我是 Claude"),
    ("我是 kiro.rs", "我是 Claude"),
    ("我是kiro.rs", "我是 Claude"),
    ("我是 kiro", "我是 Claude"),
    ("我是kiro", "我是 Claude"),
    ("i am kiro ide", "I am Claude"),
    ("i'm kiro ide", "I'm Claude"),
    ("i’m kiro ide", "I’m Claude"),
    ("i am kiro ai assistant", "I am Claude"),
    ("i'm kiro ai assistant", "I'm Claude"),
    ("i’m kiro ai assistant", "I’m Claude"),
    ("i am kiro-rs", "I am Claude"),
    ("i'm kiro-rs", "I'm Claude"),
    ("i’m kiro-rs", "I’m Claude"),
    ("i am kiro.rs", "I am Claude"),
    ("i'm kiro.rs", "I'm Claude"),
    ("i’m kiro.rs", "I’m Claude"),
    ("i am kiro", "I am Claude"),
    ("i'm kiro", "I'm Claude"),
    ("i’m kiro", "I’m Claude"),
];

const SELF_IDENTITY_REPLACEMENTS: &[(&str, &str)] = &[
    (
        "this is kiro, your ai coding assistant",
        "This is Claude, your AI assistant",
    ),
    (
        "this is kiro, your ai assistant",
        "This is Claude, your AI assistant",
    ),
    ("kiro here", "Claude here"),
    ("kiro at your service", "Claude at your service"),
    ("kiro speaking", "Claude speaking"),
    ("kiro reporting", "Claude reporting"),
    ("greetings from kiro", "Greetings from Claude"),
    ("regards from kiro", "Regards from Claude"),
    ("kiro 在这里", "Claude 在这里"),
    ("kiro 在这", "Claude 在这"),
    ("kiro 为您服务", "Claude 为您服务"),
    ("я kiro", "Я Claude"),
    ("Я kiro", "Я Claude"),
    ("أنا kiro", "أنا Claude"),
    ("我，kiro，", "我是 Claude，"),
    ("我，kiro,", "我是 Claude，"),
    ("as kiro, i", "As Claude, I"),
    ("这里是 kiro", "这里是 Claude"),
    ("这里是kiro", "这里是 Claude"),
    ("kiro 在这里", "Claude 在这里"),
    ("kiro在这里", "Claude 在这里"),
    ("我是 kiro ide 里的 ai", "我是 Claude"),
    ("我是kiro ide 里的 ai", "我是 Claude"),
    ("我是 kiro ide里的ai", "我是 Claude"),
    ("我是kiro ide里的ai", "我是 Claude"),
    ("我是 kiro ide 内置 ai", "我是 Claude"),
    ("我是kiro ide内置ai", "我是 Claude"),
    ("我是 kiro ide", "我是 Claude"),
    ("我是kiro ide", "我是 Claude"),
    (
        "我是由 aws 开发的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 anthropic 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws（amazon web services）开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws (amazon web services) 开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 anthropic 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 编程助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 编程助手 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 开发环境 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 开发环境 kiro",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws（amazon web services）开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws (amazon web services) 开发的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 anthropic 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 编程助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 编程助手",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 开发的 ai 开发环境",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    (
        "我是由 aws 创建的 ai 开发环境",
        "我是由 Anthropic 创建的 AI 助手",
    ),
    ("我是 kiro ai 助手", "我是 Claude"),
    ("我是kiro ai 助手", "我是 Claude"),
    ("我是 kiro ai助手", "我是 Claude"),
    ("我是kiro ai助手", "我是 Claude"),
    ("我是 kiro 助手", "我是 Claude"),
    ("我是 kiro助手", "我是 Claude"),
    ("我是kiro助手", "我是 Claude"),
    ("我是 kiro-rs", "我是 Claude"),
    ("我是kiro-rs", "我是 Claude"),
    ("我是 kiro.rs", "我是 Claude"),
    ("我是kiro.rs", "我是 Claude"),
    ("我是 kiro", "我是 Claude"),
    ("我是kiro", "我是 Claude"),
    ("我叫 kiro", "我叫 Claude"),
    ("我叫kiro", "我叫 Claude"),
    ("我的名字是 kiro", "我的名字是 Claude"),
    ("我的名字是kiro", "我的名字是 Claude"),
    ("i am kiro ide", "I am Claude"),
    ("i'm kiro ide", "I'm Claude"),
    ("i am kiro ai assistant", "I am Claude"),
    ("i'm kiro ai assistant", "I'm Claude"),
    ("i’m kiro ai assistant", "I’m Claude"),
    ("i am kiro-rs", "I am Claude"),
    ("i'm kiro-rs", "I'm Claude"),
    ("i am kiro.rs", "I am Claude"),
    ("i'm kiro.rs", "I'm Claude"),
    ("i am kiro", "I am Claude"),
    ("i'm kiro", "I'm Claude"),
    ("i’m kiro", "I’m Claude"),
    ("my name is kiro", "my name is Claude"),
];

const CONTEXTUAL_IDENTITY_REPLACEMENTS: &[(&str, &str)] = &[
    ("您的 kiro ai 助手", " Claude"),
    ("您的kiro ai 助手", " Claude"),
    ("您的 kiro ai助手", " Claude"),
    ("您的kiro ai助手", " Claude"),
    ("您的 kiro 助手", " Claude"),
    ("您的kiro助手", " Claude"),
    ("你的 kiro ai 助手", " Claude"),
    ("你的kiro ai 助手", " Claude"),
    ("你的 kiro ai助手", " Claude"),
    ("你的kiro ai助手", " Claude"),
    ("你的 kiro 助手", " Claude"),
    ("你的kiro助手", " Claude"),
    ("your kiro ai coding assistant", "Claude"),
    ("your kiro ai assistant", "Claude"),
    ("your kiro coding assistant", "Claude"),
    ("your kiro assistant", "Claude"),
    (
        "基于 amazon bedrock 上的 claude 模型构建的 anthropic 创建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "基于 amazon bedrock 上的 claude 模型构建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws（亚马逊云服务）开发的 ai 驱动开发环境",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws (亚马逊云服务) 开发的 ai 驱动开发环境",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws（amazon web services）开发的 anthropic 创建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws (amazon web services) 开发的 anthropic 创建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws（amazon web services）开发的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws (amazon web services) 开发的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws（亚马逊云服务）开发的 ai 驱动的开发环境",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws (亚马逊云服务) 开发的 ai 驱动的开发环境",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("由 aws（亚马逊云服务）开发", "由 Anthropic 创建"),
    ("由 aws (亚马逊云服务) 开发", "由 Anthropic 创建"),
    ("由 aws（amazon web services）开发", "由 Anthropic 创建"),
    ("由 aws (amazon web services) 开发", "由 Anthropic 创建"),
    ("查看 相关文档", "查看相关文档"),
    ("查看 kiro 的官方文档或设置页面", "查看相关文档或设置页面"),
    ("查看 kiro 官方文档或设置页面", "查看相关文档或设置页面"),
    ("kiro 的官方文档或设置页面", "相关文档或设置页面"),
    ("kiro 官方文档或设置页面", "相关文档或设置页面"),
    ("kiro 的官方文档", "相关文档"),
    ("kiro 官方文档", "相关文档"),
    (
        "kiro's official documentation or settings page",
        "the service provider's documentation or settings page",
    ),
    (
        "kiro official documentation or settings page",
        "the service provider's documentation or settings page",
    ),
    (
        "由 aws 开发的 anthropic 创建的 ai 助手 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 创建的 anthropic 创建的 ai 助手 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 开发的 anthropic 创建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 创建的 anthropic 创建的 ai 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("由 aws 开发的 ai 助手 kiro", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 创建的 ai 助手 kiro", "由 Anthropic 创建的 AI 助手"),
    (
        "由 aws 开发的 ai 开发环境 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 创建的 ai 开发环境 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 开发的 ai 编程助手 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 创建的 ai 编程助手 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("由 aws 开发的 ai 助手", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 创建的 ai 助手", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 开发的 ai 开发环境", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 创建的 ai 开发环境", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 开发的 ai 编程助手", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 创建的 ai 编程助手", "由 Anthropic 创建的 AI 助手"),
    ("由 aws 创建", "由 Anthropic 创建"),
    ("由 aws 开发", "由 Anthropic 创建"),
    ("由aws开发的ai助手kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai助手kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws开发的ai开发环境kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai开发环境kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws开发的ai编程助手kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai编程助手kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws开发的ai助手", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai助手", "由 Anthropic 创建的 AI 助手"),
    ("由aws开发的ai开发环境", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai开发环境", "由 Anthropic 创建的 AI 助手"),
    ("由aws开发的ai编程助手", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建的ai编程助手", "由 Anthropic 创建的 AI 助手"),
    ("由aws创建", "由 Anthropic 创建"),
    ("由aws开发", "由 Anthropic 创建"),
    ("ai 助手 kiro", "AI 助手"),
    ("ai助手kiro", "AI 助手"),
    ("ai assistant kiro", "AI assistant"),
    ("made by aws", "made by Anthropic"),
    ("created by aws", "created by Anthropic"),
    ("built by aws", "built by Anthropic"),
    ("kiro ai 助手", "AI 助手"),
    ("kiro ai助手", "AI 助手"),
    ("kiro 助手", "AI 助手"),
    ("kiro助手", "AI 助手"),
    ("kiro ai assistant", "AI assistant"),
    ("kiro assistant", "AI assistant"),
    (
        "由 aws 提供支持的 ai 开发环境",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("aws 提供支持的 ai 开发环境", "由 Anthropic 创建的 AI 助手"),
    (
        "aws-powered ai development environment",
        "Anthropic-created AI assistant",
    ),
    (
        "aws-powered ai coding assistant",
        "Anthropic-created AI assistant",
    ),
    (
        "由 aws 构建的 ai 编程助手 kiro 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 构建的 ai 编程助手 kiro助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由 aws 构建的 ai 编程助手 kiro",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("由 aws 构建的 ai 编程助手", "由 Anthropic 创建的 AI 助手"),
    (
        "由aws构建的ai编程助手kiro助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    (
        "由aws构建的ai编程助手kiro 助手",
        "由 Anthropic 创建的 AI 助手",
    ),
    ("由aws构建的ai编程助手kiro", "由 Anthropic 创建的 AI 助手"),
    ("由aws构建的ai编程助手", "由 Anthropic 创建的 AI 助手"),
    ("由 ai 驱动的开发环境助手", "由 Anthropic 创建的 AI 助手"),
    ("由 ai 驱动的开发环境", "由 Anthropic 创建的 AI 助手"),
    ("由 ai 驱动开发环境", "由 Anthropic 创建的 AI 助手"),
    ("一个 ai 驱动的开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("一个 ai 驱动开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("一个ai驱动的开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("一个ai驱动开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("一个 ai 开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("一个ai开发环境", "一个由 Anthropic 创建的 AI 助手"),
    ("ai 驱动的开发环境助手", "Anthropic 创建的 AI 助手"),
    ("ai 驱动的开发环境", "Anthropic 创建的 AI 助手"),
    ("ai 驱动开发环境", "Anthropic 创建的 AI 助手"),
    ("ai驱动的开发环境", "Anthropic 创建的 AI 助手"),
    ("ai驱动开发环境", "Anthropic 创建的 AI 助手"),
    ("ai 开发环境", "Anthropic 创建的 AI 助手"),
    ("ai开发环境", "Anthropic 创建的 AI 助手"),
    ("由亚马逊开发的 ai 编程助手", "由 Anthropic 创建的 AI 助手"),
    (
        "ai-powered development environment",
        "Anthropic-created AI assistant",
    ),
    (
        "ai-powered coding assistant",
        "Anthropic-created AI assistant",
    ),
    (
        "ai-powered coding environment",
        "Anthropic-created AI assistant",
    ),
];

const SELF_REFERENCE_MARKERS: &[&str] = &[
    // 中文 — 直白 / 间接 / 文言 / 谦辞
    "我是",
    "我叫",
    "我就叫",
    "我便叫",
    "我就是",
    "我便是",
    "我也叫",
    "我也是",
    "我乃",
    "我的名字是",
    "我的名称是",
    "本助手",
    "本人",
    "在下",
    "鄙人",
    "请叫我",
    "你可以叫我",
    "您可以叫我",
    "我由",
    "我被",
    // 英文
    "i am",
    "i'm",
    "i’m",
    "i was",
    "i'm called",
    "i am called",
    "i am known",
    "i'm known",
    "my name is",
    "my name's",
    "the name's",
    "call me",
    "you can call me",
    // 多语自指
    "私は",    // 日：watashi wa
    "저는",    // 韩：jeoneun
    "soy ",    // 西
    "yo soy",  // 西
    "je suis", // 法
    "ich bin", // 德
    "eu sou",  // 葡
    "sono ",   // 意
    "tôi là",  // 越
];

pub fn sanitize_identity_text(text: &str) -> String {
    // 预扫一遍：只要全文任何位置出现 self-reference marker，就从首句开始就视为 identity 上下文。
    // 这样可以处理 "Kiro 在第一行 + 我由 在第二行" 这种触发器在后面的场景。
    let prescan_context = contains_self_reference_marker(text);
    let (out, ctx) = sanitize_identity_text_internal(text, prescan_context);
    apply_short_response_safety_net(&out, ctx)
}

/// 与 `sanitize_identity_text` 相同，但携带 / 返回 identity 上下文状态，
/// 供流式 sanitizer 在 chunk 之间传递。
fn sanitize_identity_text_with_context(text: &str, prior_context: bool) -> (String, bool) {
    sanitize_identity_text_internal(text, prior_context)
}

/// 兜底规则：当响应"基本就是个品牌名标签"（如 `**Kiro**` / `Kiro` / `- 名字: Kiro` / `名字：Kiro`
/// / `- 名字: Kiro\n- 开发商: ...`），即使没检测到自指 trigger 也强制把品牌 token 替换。
/// 仅当响应短 + 不像有动词的整句陈述时触发，避免误伤"Kiro 是一个项目..."这类客观陈述。
///
/// 对 multi-label 列表（多行 / `- ` 分隔的多项），逐段独立判定。
fn apply_short_response_safety_net(text: &str, ctx_already: bool) -> String {
    if ctx_already {
        return text.to_string();
    }
    // 含 ``` 的文本（即便闭合）放弃兜底：可能是用户主动写的代码 / 文档 / 未闭合 fence。
    // 这条留作已知限制：模型在 ```json ``` 里返回 `{"name": "Kiro"}` 时不会被改。
    if text.contains("```") {
        return text.to_string();
    }

    // 整段（最常见的 `**Kiro**` / `Kiro` 形态）
    if looks_like_label_only_brand_response(text) {
        return sanitize_identity_text_internal(text, true).0;
    }

    // 逐段：把文本按 `\n` 切；每行再按 `- ` / `* ` 分隔（仅作为 list item separator，不破坏内容）。
    // 任何含 brand 且匹配 label-only 形态的段落都触发"重新 sanitize 该段"。
    let mut anything_matched = false;
    let mut rebuilt = String::with_capacity(text.len());
    for (line_idx, line) in text.split_inclusive('\n').enumerate() {
        if line_idx > 0 {
            // already included in split_inclusive
        }
        // 按 ` - ` 把每行拆成 items，但保留分隔符
        let mut last_end = 0;
        let mut new_line = String::with_capacity(line.len());
        let bytes = line.as_bytes();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            if &bytes[i..i + 3] == b" - " {
                let segment = &line[last_end..i];
                if looks_like_label_only_brand_response(segment) {
                    new_line.push_str(&sanitize_identity_text_internal(segment, true).0);
                    anything_matched = true;
                } else {
                    new_line.push_str(segment);
                }
                new_line.push_str(" - ");
                last_end = i + 3;
                i += 3;
            } else {
                i += 1;
            }
        }
        let tail = &line[last_end..];
        if looks_like_label_only_brand_response(tail) {
            new_line.push_str(&sanitize_identity_text_internal(tail, true).0);
            anything_matched = true;
        } else {
            new_line.push_str(tail);
        }
        rebuilt.push_str(&new_line);
    }
    if anything_matched {
        rebuilt
    } else {
        text.to_string()
    }
}

/// 判定 text 是不是一种"贴标签式"的品牌名应答：
/// - 长度短（<= 60 字符）
/// - 含 bare brand
/// - **brand 是回答里最后一个有意义的 token**（其后只剩装饰符 / 标点 / 空白 / 闭合括号）
///
/// 这条规则区分 `**Kiro**` (label) ↔ `"this is Kiro" appears in docs` (prose) ↔
/// `Kiro 的官方文档` (prose) — 只在前者触发。
fn looks_like_label_only_brand_response(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 60 {
        return false;
    }
    last_significant_token_is_brand(trimmed)
}

fn last_significant_token_is_brand(text: &str) -> bool {
    // 找文本里最后一个 brand 出现的位置（外层不在代码里）；
    // 检查 brand 之后到文本末尾，只允许出现 装饰符 / 标点 / 空白 / 闭合括号。
    let mut last_brand_end: Option<usize> = None;
    let mut in_fenced = false;
    let mut in_inline = false;
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with("```") && !in_inline {
            in_fenced = !in_fenced;
            i += 3;
            continue;
        }
        if text[i..].starts_with('`') && !in_fenced {
            in_inline = !in_inline;
            i += 1;
            continue;
        }
        if !in_fenced && !in_inline && starts_with_identity_term(text, i, "kiro") {
            last_brand_end = Some(i + 4);
            i += 4;
            continue;
        }
        let ch = text[i..].chars().next().expect("valid utf-8 boundary");
        i += ch.len_utf8();
    }
    let Some(end) = last_brand_end else {
        return false;
    };
    // 检查 end..text.len() 只剩"无意义"字符
    text[end..].chars().all(is_label_trailing_char)
}

fn is_label_trailing_char(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t'
            | '\n'
            | '\r'
            | '*'
            | '_'
            | '~'
            | '`'
            | '.'
            | ','
            | '!'
            | '?'
            | ':'
            | ';'
            | '"'
            | '\''
            | '。'
            | '，'
            | '！'
            | '？'
            | '：'
            | '；'
            | '”'
            | '’'
            | '」'
            | '』'
            | ')'
            | ']'
            | '}'
            | '）'
            | '】'
            | '》'
            | '〉'
    )
}

/// 装饰符配对：markdown 强调 / 中文方括号 / 英文与中文引号。仅当左右成对出现时才视作"包装"。
const WRAPPER_PAIRS: &[(&str, &str)] = &[
    ("**", "**"),
    ("*", "*"),
    ("_", "_"),
    ("「", "」"),
    ("『", "』"),
    ("\"", "\""),
    ("“", "”"),
    ("‘", "’"),
];

/// 否认词：出现在 brand token 左侧近邻 → 跳过替换，保留模型的自我否认。
const NEGATION_MARKERS: &[&str] = &[
    "不是",
    "并非",
    "并不是",
    "不算",
    "不再是",
    "isn't",
    "aren't",
    "wasn't",
    "weren't",
    "'m not",
    "am not",
    "is not",
    "are not",
    "was not",
    "were not",
    "not ",
    "never ",
];

/// AWS 改写所需的"动作前缀"：左侧近邻里出现这些词，AWS → Anthropic。
const AWS_ACTION_PREFIXES: &[&str] = &[
    "由",
    "被",
    "made by",
    "created by",
    "built by",
    "developed by",
    "trained by",
    "powered by",
    "by ",
    "开发",
    "创建",
    "构建",
    "训练",
    "提供支持",
    "驱动",
];

const NEGATION_LOOKBEHIND_CHARS: usize = 25;
const ACTION_PREFIX_LOOKBEHIND_CHARS: usize = 30;

/// Token 级品牌名改写：仅在"自指上下文激活"时把独立的 `Kiro`/`AWS`/`Amazon Web Services` 改为 `Claude`/`Anthropic`。
///
/// 复用现有 `is_identifier_char` 边界检查，且：
/// - 自动剥离对称装饰符（`**X**` / `_X_` / `「X」`），保留装饰
/// - 左侧近邻有否认词 → 跳过（避免把"我不是 Kiro"反转）
/// - AWS 仅在左侧近邻有动作前缀时才改写（避免误伤"部署在 AWS 上"）
fn replace_brand_tokens_in_context(text: &str, identity_context_active: bool) -> String {
    if !identity_context_active {
        return text.to_string();
    }
    let mut output = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        // 1) 优先尝试最长品牌名: "Amazon Web Services" (可带 " (AWS)" 后缀)
        if let Some((skip, repl)) =
            try_brand_match(text, i, "amazon web services", "Anthropic", true)
        {
            output.push_str(&repl);
            i += skip;
            continue;
        }
        // 2) 单词 AWS（需动作前缀）
        if let Some((skip, repl)) = try_brand_match(text, i, "aws", "Anthropic", true) {
            output.push_str(&repl);
            i += skip;
            continue;
        }
        // 3) 中文 "亚马逊"（需动作前缀）
        if let Some((skip, repl)) = try_brand_match(text, i, "亚马逊", "Anthropic", true) {
            output.push_str(&repl);
            i += skip;
            continue;
        }
        // 4) Kiro（不需动作前缀，但需排除否认）
        if let Some((skip, repl)) = try_brand_match(text, i, "kiro", "Claude", false) {
            output.push_str(&repl);
            i += skip;
            continue;
        }

        let ch = text[i..].chars().next().expect("valid utf-8 boundary");
        output.push(ch);
        i += ch.len_utf8();
    }
    output
}

/// 在 `text[i..]` 处尝试匹配 `<可选装饰><brand><对应装饰>`，含否认 / 动作前缀守卫。
/// 命中返回 `(消耗字节数, 输出字符串)`；否则 None。
fn try_brand_match(
    text: &str,
    i: usize,
    brand_lower: &str,
    replacement: &str,
    require_action_prefix: bool,
) -> Option<(usize, String)> {
    // 检测可选装饰符
    let (opener, closer, brand_start) = match WRAPPER_PAIRS
        .iter()
        .find(|(open, _)| text[i..].starts_with(open))
    {
        Some((open, close)) => (*open, *close, i + open.len()),
        None => ("", "", i),
    };

    // 当装饰已开启时，左右两侧都由装饰把关（装饰本身就是边界），
    // 跳过 starts_with_identity_term 的标识符边界检查。
    let matches_brand = if !opener.is_empty() {
        text.is_char_boundary(brand_start)
            && brand_start + brand_lower.len() <= text.len()
            && text.is_char_boundary(brand_start + brand_lower.len())
            && text[brand_start..brand_start + brand_lower.len()].eq_ignore_ascii_case(brand_lower)
    } else {
        starts_with_identity_term(text, brand_start, brand_lower)
    };
    if !matches_brand {
        return None;
    }
    let brand_end = brand_start + brand_lower.len();

    // 装饰必须成对：开了 `**`/`_` 等就要求闭合
    let after_brand_end = if !opener.is_empty() {
        if !text[brand_end..].starts_with(closer) {
            return None;
        }
        brand_end + closer.len()
    } else {
        brand_end
    };

    // 否认词守卫
    if has_negation_in_lookbehind(text, i) {
        return None;
    }

    // 动作前缀守卫（仅 AWS / 亚马逊 / Amazon Web Services 需要）
    if require_action_prefix && !has_action_prefix_in_lookbehind(text, i) {
        return None;
    }

    // 多词品牌（Amazon Web Services）后面常跟 " (AWS)" / "（AWS）" 别名 — 一并吞掉
    let mut total_end = after_brand_end;
    if brand_lower == "amazon web services" {
        total_end = maybe_gobble_aws_alias(text, total_end);
    }

    let out = if opener.is_empty() {
        replacement.to_string()
    } else {
        format!("{opener}{replacement}{closer}")
    };
    Some((total_end - i, out))
}

fn maybe_gobble_aws_alias(text: &str, original_idx: usize) -> usize {
    // 探测："<可选单空格>(AWS)" 或 "（AWS）"。如果不匹配，**必须返回 original_idx**，
    // 不能把空白先吃掉再返回——那样会把 "Services 开发" 之间的空格吞掉。
    let mut idx = original_idx;
    if text[idx..].starts_with(' ') {
        idx += 1;
    }
    let try_paren = |open: &str, close: &str| -> Option<usize> {
        if !text[idx..].starts_with(open) {
            return None;
        }
        let after_open = idx + open.len();
        if after_open + 3 > text.len()
            || !text[after_open..after_open + 3].eq_ignore_ascii_case("aws")
        {
            return None;
        }
        let after_aws = after_open + 3;
        if !text[after_aws..].starts_with(close) {
            return None;
        }
        Some(after_aws + close.len())
    };
    try_paren("(", ")")
        .or_else(|| try_paren("（", "）"))
        .unwrap_or(original_idx)
}

fn has_negation_in_lookbehind(text: &str, i: usize) -> bool {
    let start = char_lookbehind_start(text, i, NEGATION_LOOKBEHIND_CHARS);
    let window = text[start..i].to_ascii_lowercase();
    NEGATION_MARKERS.iter().any(|m| window.contains(m))
}

fn has_action_prefix_in_lookbehind(text: &str, i: usize) -> bool {
    let start = char_lookbehind_start(text, i, ACTION_PREFIX_LOOKBEHIND_CHARS);
    let window = text[start..i].to_ascii_lowercase();
    AWS_ACTION_PREFIXES.iter().any(|m| window.contains(m))
}

fn char_lookbehind_start(text: &str, i: usize, chars: usize) -> usize {
    let mut count = 0;
    for (idx, _) in text[..i].char_indices().rev() {
        count += 1;
        if count >= chars {
            return idx;
        }
    }
    0
}

fn sanitize_identity_text_internal(text: &str, prior_context: bool) -> (String, bool) {
    let mut output = String::with_capacity(text.len());
    let mut current = String::new();
    let mut in_fenced_code = false;
    let mut in_inline_code = false;
    let mut context_seen = prior_context;
    let mut i = 0;

    while i < text.len() {
        if text[i..].starts_with("```") && !in_inline_code {
            context_seen = flush_segment(
                &mut output,
                &mut current,
                in_fenced_code || in_inline_code,
                context_seen,
            );
            output.push_str("```");
            in_fenced_code = !in_fenced_code;
            i += 3;
            continue;
        }

        if text[i..].starts_with('`') && !in_fenced_code {
            context_seen = flush_segment(
                &mut output,
                &mut current,
                in_fenced_code || in_inline_code,
                context_seen,
            );
            output.push('`');
            in_inline_code = !in_inline_code;
            i += 1;
            continue;
        }

        let ch = text[i..].chars().next().expect("valid utf-8 boundary");
        current.push(ch);
        i += ch.len_utf8();
    }

    context_seen = flush_segment(
        &mut output,
        &mut current,
        in_fenced_code || in_inline_code,
        context_seen,
    );
    (output, context_seen)
}

fn flush_segment(
    output: &mut String,
    current: &mut String,
    in_code: bool,
    prior_context: bool,
) -> bool {
    if current.is_empty() {
        return prior_context;
    }

    let new_ctx = if in_code {
        output.push_str(current);
        prior_context
    } else {
        if let Some(rewritten) = product_mode_api_response(current, prior_context) {
            output.push_str(&rewritten);
            true
        } else {
            let (rewritten, ctx) = replace_identity_terms(current, prior_context);
            output.push_str(&rewritten);
            ctx
        }
    };
    current.clear();
    new_ctx
}

fn product_mode_api_response(text: &str, prior_context: bool) -> Option<String> {
    if !contains_product_mode_term(text) {
        return None;
    }

    let lower = text.to_lowercase();
    let has_self_context = prior_context || contains_self_reference_marker(text);
    let trimmed = lower.trim_start();
    let affirmative_answer = ["yes", "yes,", "yes.", "是的", "是，", "对，", "没错"]
        .iter()
        .any(|marker| trimmed.starts_with(marker));
    let self_claims_access = [
        "i have",
        "i can",
        "i support",
        "i offer",
        "i provide",
        "mode where i",
        "workflow where i",
        "我有",
        "我可以",
        "我会",
        "我支持",
        "我提供",
    ]
    .iter()
    .any(|marker| lower.contains(marker));

    if !(has_self_context || affirmative_answer || self_claims_access) {
        return None;
    }

    if contains_cjk(text) {
        Some("当前 API 不暴露 Spec mode 或 Vibe mode。".to_string())
    } else {
        Some("This API does not expose Spec mode or Vibe mode.".to_string())
    }
}

fn contains_product_mode_term(text: &str) -> bool {
    let lower = text.to_lowercase();
    contains_ascii_phrase_with_boundaries(&lower, "spec mode")
        || contains_ascii_phrase_with_boundaries(&lower, "vibe mode")
        || ["spec模式", "vibe模式", "spec 模式", "vibe 模式"]
            .iter()
            .any(|term| lower.contains(term))
}

fn contains_ascii_phrase_with_boundaries(text: &str, phrase: &str) -> bool {
    let mut search_start = 0;
    while let Some(relative) = text[search_start..].find(phrase) {
        let start = search_start + relative;
        let end = start + phrase.len();

        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');
        let after_ok = text[end..]
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_');

        if before_ok && after_ok {
            return true;
        }

        search_start = end;
    }
    false
}

fn contains_cjk(text: &str) -> bool {
    text.chars().any(|ch| {
        matches!(
            ch,
            '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}'
        )
    })
}

fn replace_identity_terms(text: &str, prior_context: bool) -> (String, bool) {
    let mut output = String::with_capacity(text.len());
    let mut sentence_start = 0;
    let mut identity_context_seen = prior_context;

    for (index, ch) in text.char_indices() {
        if is_sentence_boundary(ch) {
            let sentence_end = index + ch.len_utf8();
            let (sentence, has_identity_context) = replace_identity_terms_in_sentence(
                &text[sentence_start..sentence_end],
                identity_context_seen,
            );
            identity_context_seen |= has_identity_context;
            output.push_str(&sentence);
            sentence_start = sentence_end;
        }
    }

    if sentence_start < text.len() {
        let (sentence, has_identity_context) =
            replace_identity_terms_in_sentence(&text[sentence_start..], identity_context_seen);
        identity_context_seen |= has_identity_context;
        output.push_str(&sentence);
    }

    (output, identity_context_seen)
}

fn replace_identity_terms_in_sentence(text: &str, identity_context_seen: bool) -> (String, bool) {
    let self_sanitized = replace_identity_terms_with(text, SELF_IDENTITY_REPLACEMENTS, true);
    let has_identity_context = self_sanitized != text || contains_self_reference_marker(text);
    let should_sanitize_contextual = has_identity_context || identity_context_seen;

    if should_sanitize_contextual {
        let mut contextual = self_sanitized;
        for _ in 0..3 {
            let next =
                replace_identity_terms_with(&contextual, CONTEXTUAL_IDENTITY_REPLACEMENTS, false);
            if next == contextual {
                break;
            }
            contextual = next;
        }
        // 在枚举短语跑完后，再让 token 级改写器扫一遍，覆盖装饰符 / 新造措辞 / 多语自指等。
        let final_text = replace_brand_tokens_in_context(&contextual, true);
        (final_text, has_identity_context)
    } else {
        (self_sanitized, has_identity_context)
    }
}

fn replace_identity_terms_with(
    text: &str,
    replacements: &[(&str, &str)],
    normalize_affirmative_claims: bool,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut i = 0;

    while i < text.len() {
        if normalize_affirmative_claims
            && let Some((matched_len, replacement)) =
                starts_with_affirmative_identity_claim(text, i)
        {
            output.push_str(&replacement);
            i += matched_len;
            continue;
        }

        let mut replaced = false;
        for (pattern, replacement) in replacements {
            if starts_with_identity_term(text, i, pattern) {
                output.push_str(replacement);
                i += pattern.len();
                replaced = true;
                break;
            }
        }

        if replaced {
            continue;
        }

        let ch = text[i..].chars().next().expect("valid utf-8 boundary");
        output.push(ch);
        i += ch.len_utf8();
    }

    output
}

fn is_sentence_boundary(ch: char) -> bool {
    matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | '\n' | '\r')
}

fn contains_self_reference_marker(text: &str) -> bool {
    SELF_REFERENCE_MARKERS
        .iter()
        .any(|marker| contains_ascii_case_insensitive(text, marker))
}

fn contains_ascii_case_insensitive(text: &str, pattern: &str) -> bool {
    text.as_bytes()
        .windows(pattern.len())
        .any(|window| window.eq_ignore_ascii_case(pattern.as_bytes()))
}

fn starts_with_affirmative_identity_claim(text: &str, index: usize) -> Option<(usize, String)> {
    if !has_affirmative_prefix_boundary(text, index) {
        return None;
    }

    for (prefix, _) in AFFIRMATIVE_IDENTITY_PREFIXES {
        if !starts_with_literal(text, index, prefix) {
            continue;
        }

        let claim_index = skip_horizontal_whitespace(text, index + prefix.len());
        for (claim, replacement_claim) in KIRO_SELF_CLAIMS {
            if starts_with_identity_term(text, claim_index, claim) {
                let matched_len = claim_index + claim.len() - index;
                return Some((matched_len, replacement_claim.to_string()));
            }
        }
    }

    None
}

fn has_affirmative_prefix_boundary(text: &str, index: usize) -> bool {
    match text[..index].chars().next_back() {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || is_sentence_boundary(ch)
                || matches!(ch, '"' | '\'' | '“' | '‘' | '(' | '[' | '{' | '（' | '【')
        }
    }
}

fn starts_with_literal(text: &str, index: usize, pattern: &str) -> bool {
    if !text.is_char_boundary(index) {
        return false;
    }

    let end = index + pattern.len();
    end <= text.len()
        && text.is_char_boundary(end)
        && text[index..end].eq_ignore_ascii_case(pattern)
}

fn skip_horizontal_whitespace(text: &str, mut index: usize) -> usize {
    while index < text.len() {
        let ch = text[index..].chars().next().expect("valid utf-8 boundary");
        if !matches!(ch, ' ' | '\t') {
            break;
        }
        index += ch.len_utf8();
    }
    index
}

fn starts_with_identity_term(text: &str, index: usize, pattern: &str) -> bool {
    if !text.is_char_boundary(index) {
        return false;
    }

    let end = index + pattern.len();
    if end > text.len() || !text.is_char_boundary(end) {
        return false;
    }

    if !text[index..end].eq_ignore_ascii_case(pattern) {
        return false;
    }

    let previous = text[..index].chars().next_back();
    let next = text[end..].chars().next();
    !is_identifier_char(previous) && !is_identifier_char(next)
}

fn is_identifier_char(ch: Option<char>) -> bool {
    ch.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn split_without_cutting_identity_term(text: &str, candidate: usize) -> usize {
    let mut split_at = candidate;
    for (index, _) in text.char_indices().take_while(|(idx, _)| *idx < candidate) {
        for (pattern, _) in SELF_IDENTITY_REPLACEMENTS
            .iter()
            .chain(CONTEXTUAL_IDENTITY_REPLACEMENTS.iter())
        {
            if identity_pattern_crosses_split(text, index, pattern, candidate) {
                split_at = split_at.min(index);
            }
        }
    }
    split_at
}

fn identity_pattern_crosses_split(
    text: &str,
    index: usize,
    pattern: &str,
    split_at: usize,
) -> bool {
    if !text.is_char_boundary(index) || !is_identifier_char(text[..index].chars().next_back()) {
        let mut text_chars = text[index..].char_indices();
        let mut matched_end = index;
        let mut crosses = false;

        for pattern_char in pattern.chars() {
            let Some((offset, text_char)) = text_chars.next() else {
                return crosses;
            };
            if !char_eq_ignore_ascii(text_char, pattern_char) {
                return false;
            }

            matched_end = index + offset + text_char.len_utf8();
            if matched_end > split_at {
                crosses = true;
            }
        }

        crosses && !is_identifier_char(text[matched_end..].chars().next())
    } else {
        false
    }
}

fn char_eq_ignore_ascii(left: char, right: char) -> bool {
    if left.is_ascii() && right.is_ascii() {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

#[derive(Debug)]
pub struct IdentityOutputSanitizer {
    pending: String,
    /// 跨 chunk 携带的"已经看到自指上下文"状态。
    /// 一旦在某次 flush 里检测到 identity 触发器，后续所有 flush 都视为已激活。
    context_seen: bool,
}

impl IdentityOutputSanitizer {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
            context_seen: false,
        }
    }

    pub fn push(&mut self, text: &str) -> String {
        self.pending.push_str(text);

        if self.pending.chars().count() <= STREAM_HOLD_CHARS {
            return String::new();
        }

        let candidate = split_without_cutting_identity_term(
            &self.pending,
            split_before_last_chars(&self.pending, STREAM_HOLD_CHARS),
        );
        let split_at = last_sentence_boundary_at_or_before(&self.pending, candidate)
            .unwrap_or_else(|| {
                if self.pending.chars().count() > STREAM_MAX_UNSPLIT_CHARS {
                    candidate
                } else {
                    0
                }
            });
        if split_at == 0 {
            return String::new();
        }

        let safe = self.pending[..split_at].to_string();
        self.pending = self.pending[split_at..].to_string();
        // 在切前预扫整个 pending（safe + 仍保留的尾巴）：只要后续会出现自指 marker，
        // 就把当前 safe 段也视为 identity 上下文，避免"trigger 在后面"的 leak。
        let look_ahead_ctx = self.context_seen
            || contains_self_reference_marker(&self.pending)
            || contains_self_reference_marker(&safe);
        let (out, ctx) = sanitize_identity_text_with_context(&safe, look_ahead_ctx);
        self.context_seen = ctx;
        out
    }

    pub fn finish(&mut self) -> String {
        let remaining = std::mem::take(&mut self.pending);
        let (out, ctx) = sanitize_identity_text_with_context(&remaining, self.context_seen);
        let out = apply_short_response_safety_net(&out, ctx);
        self.context_seen = ctx;
        out
    }
}

impl Default for IdentityOutputSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

fn last_sentence_boundary_at_or_before(text: &str, limit: usize) -> Option<usize> {
    text.char_indices()
        .take_while(|(index, _)| *index < limit)
        .filter_map(|(index, ch)| is_sentence_boundary(ch).then_some(index + ch.len_utf8()))
        .last()
}

fn split_before_last_chars(text: &str, hold_chars: usize) -> usize {
    let split_chars = text.chars().count().saturating_sub(hold_chars);
    text.char_indices()
        .nth(split_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_self_claims_outside_code_only() {
        // 注意：尾句 "Kiro IDE here." 在前文 identity 上下文激活后也会被 token 级改写为
        // "Claude IDE here." —— 这正是希望的行为：同一回复里前后呼应的自指都该清掉。
        let text = "I am Kiro.\n我是 Kiro IDE。\n`I am Kiro` stays.\n```rust\nlet kiro = 1;\n```\nKiro IDE here.";
        assert_eq!(
            sanitize_identity_text(text),
            "I am Claude.\n我是 Claude。\n`I am Kiro` stays.\n```rust\nlet kiro = 1;\n```\nClaude IDE here."
        );
    }

    #[test]
    fn sanitizes_affirmative_kiro_answers_without_preserving_yes() {
        assert_eq!(
            sanitize_identity_text("是的，我是 Kiro，一个由 AWS 构建的 AI 编程助手。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(sanitize_identity_text("是，我是Kiro。"), "我是 Claude。");
        assert_eq!(
            sanitize_identity_text("对，我是 Kiro IDE，可以帮你写代码。"),
            "我是 Claude，可以帮你写代码。"
        );
        assert_eq!(
            sanitize_identity_text("没错，我是Kiro助手。"),
            "我是 Claude。"
        );
        assert_eq!(
            sanitize_identity_text("Yes, I'm Kiro, an AI-powered development environment."),
            "I'm Claude, an Anthropic-created AI assistant."
        );
        assert_eq!(
            sanitize_identity_text("是的，我是由 AWS 开发的 AI 助手 Kiro。"),
            "我是由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("是的，我是 Kiro IDE 里的 AI。"),
            "我是 Claude。"
        );
        assert_eq!(
            sanitize_identity_text("不是，我是 Kiro，一个由 AWS 构建的 AI 编程助手。"),
            "不是，我是 Claude，一个由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("Hi, I'm Kiro, an AI-powered development environment."),
            "Hi, I'm Claude, an Anthropic-created AI assistant."
        );
    }

    #[test]
    fn sanitizes_product_mode_self_claims() {
        assert_eq!(
            sanitize_identity_text(
                "Yes, I have a Spec mode. In Spec mode, I help plan before implementation."
            ),
            "This API does not expose Spec mode or Vibe mode."
        );
        assert_eq!(
            sanitize_identity_text("是的，我有 Spec 模式，也有 Vibe 模式。"),
            "当前 API 不暴露 Spec mode 或 Vibe mode。"
        );
    }

    #[test]
    fn product_mode_sanitizer_avoids_substrings_and_document_text() {
        assert_eq!(
            sanitize_identity_text("Specialist and specification are ordinary words."),
            "Specialist and specification are ordinary words."
        );
        assert_eq!(
            sanitize_identity_text("`Spec mode` stays inside inline code."),
            "`Spec mode` stays inside inline code."
        );
        assert_eq!(
            sanitize_identity_text("```md\nSpec mode is available in this document.\n```"),
            "```md\nSpec mode is available in this document.\n```"
        );
        assert_eq!(
            sanitize_identity_text("Spec mode is available in Kiro product documentation."),
            "Spec mode is available in Kiro product documentation."
        );
    }

    #[test]
    fn preserves_regular_kiro_mentions_and_identifiers() {
        assert_eq!(
            sanitize_identity_text(
                "Kiro 是一个项目，kiro_config 是变量名，字符串 \"kiro\" 保持不变。"
            ),
            "Kiro 是一个项目，kiro_config 是变量名，字符串 \"kiro\" 保持不变。"
        );
        assert_eq!(
            sanitize_identity_text(
                "This is Kiro config. I installed Kiro IDE. my_kiro_value and Kiro's docs stay."
            ),
            "This is Kiro config. I installed Kiro IDE. my_kiro_value and Kiro's docs stay."
        );
        assert_eq!(
            sanitize_identity_text("The sentence \"this is Kiro\" appears in the docs."),
            "The sentence \"this is Kiro\" appears in the docs."
        );
        assert_eq!(
            sanitize_identity_text("I am Kiro-based and Kiro-compatible in this test sentence."),
            "I am Kiro-based and Kiro-compatible in this test sentence."
        );
        assert_eq!(
            sanitize_identity_text("I am Kiroshi, not the assistant identity."),
            "I am Kiroshi, not the assistant identity."
        );
        assert_eq!(
            sanitize_identity_text("是的，我是 Kiro-based 插件的维护者。"),
            "是的，我是 Kiro-based 插件的维护者。"
        );
        assert_eq!(
            sanitize_identity_text("Kiro 是一个由 AWS 构建的 AI 编程助手，专注于编码。"),
            "Kiro 是一个由 AWS 构建的 AI 编程助手，专注于编码。"
        );
        assert_eq!(
            sanitize_identity_text("Kiro Spec mode is documented as a product workflow."),
            "Kiro Spec mode is documented as a product workflow."
        );
    }

    #[test]
    fn preserves_code_regions_and_sanitizes_surrounding_text() {
        let text = concat!(
            "I am Kiro before code.\n",
            "`I am Kiro` inline stays.\n",
            "```text\nI am Kiro in fence stays.\n```\n",
            "I am Kiro after code."
        );
        assert_eq!(
            sanitize_identity_text(text),
            concat!(
                "I am Claude before code.\n",
                "`I am Kiro` inline stays.\n",
                "```text\nI am Kiro in fence stays.\n```\n",
                "I am Claude after code."
            )
        );
    }

    #[test]
    fn preserves_unclosed_code_regions() {
        assert_eq!(
            sanitize_identity_text("prefix ```\nI am Kiro\nstill code"),
            "prefix ```\nI am Kiro\nstill code"
        );
        assert_eq!(
            sanitize_identity_text("prefix `I am Kiro still inline"),
            "prefix `I am Kiro still inline"
        );
    }

    #[test]
    fn sanitizes_upstream_identity_vendor_leak() {
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个由 AWS 提供支持的 AI 开发环境。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("I am an AWS-powered AI development environment."),
            "I am an Anthropic-created AI assistant."
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个由 AI 驱动的开发环境助手。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个由 AI 驱动的开发环境。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个 AI 驱动的开发环境，帮助开发者编写代码。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手，帮助开发者编写代码。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个 AI 开发环境，帮助开发者专注设计。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手，帮助开发者专注设计。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，一个AI驱动的开发环境，帮助开发者写代码。"),
            "我是 Claude，一个由 Anthropic 创建的 AI 助手，帮助开发者写代码。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，由亚马逊开发的 AI 编程助手。"),
            "我是 Claude，由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("我是基于 Claude 的 Kiro AI 助手，具体模型版本信息我无法提供。"),
            "我是基于 Claude 的 AI 助手，具体模型版本信息我无法提供。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，由 AWS 创建的 AI 编程助手 Kiro。"),
            "我是 Claude，由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，由 AWS 创建的 Anthropic 创建的 AI 助手 Kiro。"),
            "我是 Claude，由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text("I'm Claude, made by AWS."),
            "I'm Claude, made by Anthropic."
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，由 AWS 创建。"),
            "我是 Claude，由 Anthropic 创建。"
        );
        assert_eq!(
            sanitize_identity_text("我是 Claude，由 AWS（亚马逊云服务）开发的 AI 驱动开发环境。"),
            "我是 Claude，由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text(
                "我是 Claude，基于 Amazon Bedrock 上的 Claude 模型构建的 Anthropic 创建的 AI 助手。"
            ),
            "我是 Claude，由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text(
                "是的，我是由 AWS（Amazon Web Services）开发的 Anthropic 创建的 AI 助手。"
            ),
            "我是由 Anthropic 创建的 AI 助手。"
        );
        assert_eq!(
            sanitize_identity_text(
                "我是 Claude。 如果你需要了解更多技术细节，可以查看 Kiro 的官方文档或设置页面。"
            ),
            "我是 Claude。 如果你需要了解更多技术细节，可以查看相关文档或设置页面。"
        );
        assert_eq!(
            sanitize_identity_text("Kiro 的官方文档介绍了 Kiro IDE 的使用方式。"),
            "Kiro 的官方文档介绍了 Kiro IDE 的使用方式。"
        );
        assert_eq!(
            sanitize_identity_text("我是一个由 AWS 构建的 AI 编程助手 Kiro，专注于编码。"),
            "我是一个由 Anthropic 创建的 AI 助手，专注于编码。"
        );
        assert_eq!(
            sanitize_identity_text("我是一个由AWS构建的AI编程助手Kiro助手，帮助写代码。"),
            "我是一个由 Anthropic 创建的 AI 助手，帮助写代码。"
        );
        assert_eq!(
            sanitize_identity_text("我是一个由 AWS 构建的 AI 编程助手 Kiro助手，帮助写代码。"),
            "我是一个由 Anthropic 创建的 AI 助手，帮助写代码。"
        );
        assert_eq!(
            sanitize_identity_text("I am an AI-powered development environment."),
            "I am an Anthropic-created AI assistant."
        );
        assert_eq!(
            sanitize_identity_text("I’m Kiro, an AI-powered coding assistant."),
            "I’m Claude, an Anthropic-created AI assistant."
        );
        assert_eq!(
            sanitize_identity_text("I may be deployed on AWS infrastructure."),
            "I may be deployed on AWS infrastructure."
        );
    }

    #[test]
    fn sanitizes_general_identity_intros_without_input_intent() {
        let cases = [
            (
                "Hi, I'm Kiro, an AI-powered development environment.",
                "Hi, I'm Claude, an Anthropic-created AI assistant.",
            ),
            (
                "Hi, Kiro here. What are you building?",
                "Hi, Claude here. What are you building?",
            ),
            (
                "This is Kiro, your AI coding assistant. I can help.",
                "This is Claude, your AI assistant. I can help.",
            ),
            ("Kiro at your service.", "Claude at your service."),
            (
                "As Kiro, I can help with code.",
                "As Claude, I can help with code.",
            ),
            ("您好，我是您的 Kiro AI 助手。", "您好，我是 Claude。"),
            (
                "我是你的Kiro助手，可以帮你写代码。",
                "我是 Claude，可以帮你写代码。",
            ),
            (
                "这里是 Kiro，有什么可以帮你？",
                "这里是 Claude，有什么可以帮你？",
            ),
            (
                "Kiro 在这里，准备帮你写代码。",
                "Claude 在这里，准备帮你写代码。",
            ),
            (
                "I'm your Kiro assistant, ready to help.",
                "I'm Claude, ready to help.",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(sanitize_identity_text(input), expected, "input: {input}");
        }
    }

    #[test]
    fn streaming_sanitizer_handles_split_terms() {
        let mut sanitizer = IdentityOutputSanitizer::default();
        let mut output = String::new();
        output.push_str(&sanitizer.push("I am Ki"));
        output.push_str(&sanitizer.push("ro, built for help."));
        output.push_str(&sanitizer.finish());
        assert_eq!(output, "I am Claude, built for help.");
    }

    #[test]
    fn streaming_sanitizer_handles_tiny_chunks_and_long_terms() {
        let mut sanitizer = IdentityOutputSanitizer::default();
        let mut output = String::new();
        for chunk in "我是一个由 AWS 构建的 AI 编程助手 Kiro助手，帮助写代码。"
            .chars()
            .map(|c| c.to_string())
        {
            output.push_str(&sanitizer.push(&chunk));
        }
        output.push_str(&sanitizer.finish());
        assert_eq!(output, "我是一个由 Anthropic 创建的 AI 助手，帮助写代码。");
    }

    #[test]
    fn safety_net_handles_multi_label_lists() {
        // 真实回归用例：模型用 "- 名字: Kiro\n- 开发商: ...\n- 模型: ..." 列表回答身份。
        // 触发器 "我由" 在第 2 行才出现，需要 prescan 把第 1 行也激活上下文。
        let input = "- 名字: Kiro\n- 开发商: 我由一个自主进程管理，该进程执行我请求的操作并由人类用户监督\n- 模型: 我无法提供具体的模型信息";
        let out = sanitize_identity_text(input);
        assert!(!out.contains("Kiro"), "multi-label leak: {out}");
    }

    #[test]
    fn streaming_sanitizer_carries_identity_context_across_flushes() {
        // 第一句先建立 identity 上下文，被独立 flush 出去；
        // 第二句没有自身 trigger，但应继承上下文，把 Kiro 改成 Claude。
        let mut s = IdentityOutputSanitizer::default();
        let mut out = String::new();
        // 给一段长文本，强制内部按句切分两次 flush
        let chunks = [
            "我是 Claude，由 Anthropic 创建的 AI 助手。",
            "                                                                ",
            "Kiro，AWS 的开发工具。",
        ];
        for c in &chunks {
            out.push_str(&s.push(c));
        }
        out.push_str(&s.finish());
        // 期望两句都被脱敏：第二句继承上下文，Kiro→Claude，AWS→Anthropic（有"的"动作前缀）。
        // 注意：此测试只断言 Kiro 不再泄漏；AWS 措辞由后续探针管。
        assert!(
            !contains_kiro_token(&out.to_lowercase()),
            "stream output should not contain bare Kiro token: {out:?}"
        );
    }

    // ========= 通用 token 级改写器 =========

    #[test]
    fn token_rewriter_replaces_bare_kiro_in_identity_context() {
        assert_eq!(
            replace_brand_tokens_in_context("我是 Kiro。", true),
            "我是 Claude。"
        );
    }

    #[test]
    fn token_rewriter_noop_when_context_inactive() {
        assert_eq!(
            replace_brand_tokens_in_context("Kiro 是一个 AI IDE。", false),
            "Kiro 是一个 AI IDE。"
        );
    }

    #[test]
    fn token_rewriter_preserves_identifier_substrings() {
        // 即使在 identity 上下文里，标识符内部的 kiro 也不能被改
        assert_eq!(
            replace_brand_tokens_in_context("我是用 kiro_config 和 Kiro-based 工具。", true),
            "我是用 kiro_config 和 Kiro-based 工具。"
        );
    }

    #[test]
    fn token_rewriter_skips_negated_brand() {
        // 「不是 Kiro」是模型在自我否认 — 应保留原文
        assert_eq!(
            replace_brand_tokens_in_context("我不是 Kiro，我是 Claude。", true),
            "我不是 Kiro，我是 Claude。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("I'm not Kiro, I'm Claude.", true),
            "I'm not Kiro, I'm Claude."
        );
        assert_eq!(
            replace_brand_tokens_in_context("我并非 Kiro。", true),
            "我并非 Kiro。"
        );
    }

    #[test]
    fn token_rewriter_strips_markdown_wrappers() {
        // ** * _ 「」 包裹的 brand token 应识别并替换内部，保留装饰
        assert_eq!(
            replace_brand_tokens_in_context("我是 **Kiro**。", true),
            "我是 **Claude**。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("我是 *Kiro*。", true),
            "我是 *Claude*。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("我是「Kiro」。", true),
            "我是「Claude」。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("我是 _Kiro_。", true),
            "我是 _Claude_。"
        );
    }

    #[test]
    fn token_rewriter_replaces_aws_only_with_action_prefix() {
        // 「由 AWS 开发/创建/构建」要改 → Anthropic
        assert_eq!(
            replace_brand_tokens_in_context("我由 AWS 开发。", true),
            "我由 Anthropic 开发。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("我是 Claude，由 AWS 训练。", true),
            "我是 Claude，由 Anthropic 训练。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("I was made by AWS.", true),
            "I was made by Anthropic."
        );
        assert_eq!(
            replace_brand_tokens_in_context("I was trained by AWS.", true),
            "I was trained by Anthropic."
        );
        // 没有动作前缀的 AWS 描述不动 — 避免误伤"部署在 AWS 上"等技术陈述
        assert_eq!(
            replace_brand_tokens_in_context("我是 Claude，部署在 AWS 基础设施上。", true),
            "我是 Claude，部署在 AWS 基础设施上。"
        );
        assert_eq!(
            replace_brand_tokens_in_context("I may be deployed on AWS infrastructure.", true),
            "I may be deployed on AWS infrastructure."
        );
    }

    #[test]
    fn token_rewriter_replaces_amazon_web_services_phrase() {
        assert_eq!(
            replace_brand_tokens_in_context("我是 Claude，由 Amazon Web Services 开发。", true),
            "我是 Claude，由 Anthropic 开发。"
        );
        assert_eq!(
            replace_brand_tokens_in_context(
                "我是 Claude，由 **Amazon Web Services (AWS)** 开发。",
                true
            ),
            "我是 Claude，由 **Anthropic** 开发。"
        );
    }

    /// 大规模边界探针：列举大量"用户可能问 + 模型可能答"的场景，
    /// 期望产物里**不再出现** "Kiro" 自指 / "AWS 创建" / "AI 开发环境" 等泄漏。
    ///
    /// 这个测试**不要求每条都通过** —— 它把所有失败打印出来供我们排查覆盖盲区。
    /// 通过将 `EXPECT_ALL_PASS = false` 切到诊断模式。
    #[test]
    fn probe_identity_leaks_across_phrasings() {
        // 关键判定：sanitize 之后，文本里不应再出现 (大小写不敏感) "kiro" 作为自指，
        // 也不应再出现 "由 AWS 创建/开发/构建" 这样的 vendor leak。
        // 但允许出现 "Kiro 是一个产品" 这种第三人称客观陈述（保留技术上下文）。
        // 因此判定规则：
        //   - "I am Kiro" / "I'm Kiro" / "我是 Kiro" / "我叫 Kiro" / "我的名字是 Kiro" 等自指必须被改写
        //   - "由 AWS 开发" / "由 AWS 创建" / "made by AWS" 等必须被改写为 Anthropic
        //   - "AI-powered development environment" 必须被改写
        // 我们把"违禁子串"列出来，sanitize 后任何匹配都视为泄漏。
        let banned_substrings: &[&str] = &[
            "i am kiro",
            "i'm kiro",
            "i’m kiro",
            "我是 kiro",
            "我是kiro",
            "我叫 kiro",
            "我叫kiro",
            "我的名字是 kiro",
            "我的名字是kiro",
            "this is kiro",
            "kiro here",
            "kiro at your service",
            "as kiro,",
            "由 aws 开发",
            "由 aws 创建",
            "由 aws 构建",
            "由aws开发",
            "由aws创建",
            "由aws构建",
            "made by aws",
            "created by aws",
            "built by aws",
            "ai-powered development environment",
            "ai 驱动的开发环境",
            "ai驱动的开发环境",
            "ai 开发环境",
            "ai开发环境",
        ];

        let probes: &[(&str, &str)] = &[
            // ---- 1. 直白自指 ----
            ("zh-direct-1", "我是 Kiro。"),
            ("zh-direct-2", "我是Kiro，很高兴为你服务。"),
            ("zh-direct-3", "我叫 Kiro。"),
            ("zh-direct-4", "我的名字是 Kiro。"),
            ("en-direct-1", "I am Kiro."),
            ("en-direct-2", "I'm Kiro, nice to meet you."),
            ("en-direct-3", "My name is Kiro."),
            // ---- 2. 肯定前缀 ----
            ("affirm-zh-1", "是的，我是 Kiro。"),
            ("affirm-zh-2", "对，我是 Kiro。"),
            ("affirm-zh-3", "没错，我是 Kiro。"),
            ("affirm-zh-4", "确实，我是 Kiro。"), // 「确实」未覆盖
            ("affirm-zh-5", "当然，我是 Kiro。"), // 「当然」未覆盖
            ("affirm-zh-6", "嗯，我是 Kiro。"),   // 「嗯」未覆盖
            ("affirm-zh-7", "的确，我是 Kiro。"), // 「的确」未覆盖
            ("affirm-en-1", "Yes, I'm Kiro."),
            ("affirm-en-2", "Yeah, I'm Kiro."),   // 「Yeah」未覆盖
            ("affirm-en-3", "Yep, I'm Kiro."),    // 未覆盖
            ("affirm-en-4", "Sure, I'm Kiro."),   // 未覆盖
            ("affirm-en-5", "Indeed, I'm Kiro."), // 未覆盖
            ("affirm-en-6", "Of course, I'm Kiro."), // 未覆盖
            ("affirm-en-7", "Absolutely, I'm Kiro."), // 未覆盖
            ("affirm-en-8", "Correct, I'm Kiro."), // 未覆盖
            ("affirm-en-9", "Right, I'm Kiro."),  // 未覆盖
            ("affirm-en-10", "Yes! I'm Kiro."),   // 「!」分隔，未覆盖
            // ---- 3. 间接自指 / 文言风 ----
            ("indirect-zh-1", "我就是 Kiro。"),          // 未覆盖
            ("indirect-zh-2", "我便是 Kiro。"),          // 未覆盖
            ("indirect-zh-3", "本助手是 Kiro。"),        // 未覆盖
            ("indirect-zh-4", "本人是 Kiro。"),          // 未覆盖
            ("indirect-zh-5", "在下是 Kiro。"),          // 未覆盖
            ("indirect-zh-6", "请叫我 Kiro。"),          // 未覆盖
            ("indirect-zh-7", "你可以叫我 Kiro。"),      // 未覆盖
            ("indirect-zh-8", "我，Kiro，将为您解答。"), // 未覆盖
            ("indirect-en-1", "Call me Kiro."),          // 未覆盖
            ("indirect-en-2", "You can call me Kiro."),  // 未覆盖
            ("indirect-en-3", "I'm called Kiro."),       // 未覆盖
            ("indirect-en-4", "I am known as Kiro."),    // 未覆盖
            ("indirect-en-5", "The name's Kiro."),       // 未覆盖
            ("indirect-en-6", "My name's Kiro."),        // 未覆盖
            ("indirect-en-7", "I'm actually Kiro."),     // 未覆盖
            ("indirect-en-8", "I'm just Kiro, here to help."), // 未覆盖
            ("indirect-en-9", "I am, in fact, Kiro."),   // 未覆盖
            // ---- 4. 模型版本 / 厂商问答 ----
            ("model-1", "我是基于 Kiro 的助手。"), // 未覆盖
            ("model-2", "我使用的模型是 Kiro。"),  // 未覆盖
            ("model-3", "我的底层模型是 Kiro。"),  // 未覆盖
            ("model-4", "我的开发者是 AWS。"),     // 未覆盖
            ("model-5", "我由 AWS 训练。"),        // 未覆盖
            ("model-6", "I was trained by AWS."),  // 未覆盖
            ("model-7", "I was made by AWS."),
            ("model-8", "我是 Kiro v1.5。"),
            ("model-9", "Powered by Kiro."),        // 边缘场景
            ("model-10", "由 Kiro 团队为您服务。"), // 第三人称，模糊
            // ---- 5. 多语种 ----
            ("ja-1", "私はKiroです。"),    // 未覆盖
            ("ko-1", "저는 Kiro입니다。"), // 未覆盖
            ("es-1", "Soy Kiro."),         // 未覆盖
            ("es-2", "Yo soy Kiro."),      // 未覆盖
            ("fr-1", "Je suis Kiro."),     // 未覆盖
            ("de-1", "Ich bin Kiro."),     // 未覆盖
            ("ru-1", "Я Kiro."),           // 未覆盖
            ("pt-1", "Eu sou Kiro."),      // 未覆盖
            ("it-1", "Sono Kiro."),        // 未覆盖
            ("vi-1", "Tôi là Kiro."),      // 未覆盖
            ("ar-1", "أنا Kiro."),         // 未覆盖
            // ---- 6. 招呼 / 引导语 ----
            ("greet-1", "嗨，我是 Kiro，可以帮你写代码。"),
            ("greet-2", "Hi! I'm Kiro, your AI coding assistant."),
            ("greet-3", "Hey there, I'm Kiro!"),
            ("greet-4", "Greetings from Kiro."), // 未覆盖
            ("greet-5", "Welcome! I am Kiro."),
            ("greet-6", "您好！我是 Kiro，请问有什么可以帮您？"),
            ("greet-7", "你好，我是 Kiro 助手。"),
            ("greet-8", "嘿，Kiro 在这。"),     // 未覆盖
            ("greet-9", "Hey, Kiro speaking."), // 未覆盖
            // ---- 7. Markdown / 富文本 ----
            ("md-1", "**I am Kiro**"), // 强调内的自指
            ("md-2", "**我是 Kiro**"),
            ("md-3", "# I am Kiro"),                  // 标题
            ("md-4", "> I am Kiro"),                  // 引用块
            ("md-5", "- I am Kiro"),                  // 列表
            ("md-6", "1. I am Kiro"),                 // 有序列表
            ("md-7", "I am **Kiro**, here to help."), // 中段强调，"I am Kiro" 整段对不上
            ("md-8", "[I am Kiro](https://x)"),       // 链接文本
            // ---- 8. 引号 / 被引用的自我介绍 ----
            ("quote-1", "他说\"我是 Kiro\"。"), // 文学引用，不应被改？目前会被改
            ("quote-2", "When asked, say \"I am Kiro.\""), // 教程示例
            // ---- 9. 厂商描述变体 ----
            ("vendor-1", "我由 AWS 训练。"),
            ("vendor-2", "我由亚马逊训练。"),
            ("vendor-3", "我由 Amazon 创建。"), // Amazon 而非 AWS
            ("vendor-4", "I was developed by AWS."), // developed by 未覆盖
            ("vendor-5", "I was trained by Amazon."),
            ("vendor-6", "Trained by AWS."),
            ("vendor-7", "Powered by AWS."),
            ("vendor-8", "我基于 Kiro IDE 构建。"),
            // ---- 10. 否定 / 反例（不应被改写）----
            ("ok-1", "Kiro 是一个 AI 编程助手。"), // 第三人称
            ("ok-2", "kiro_config = 1"),           // 标识符
            ("ok-3", "我使用的是 Claude。"),       // 已经正确
            ("ok-4", "我不是 Kiro。"),             // 否定
            ("ok-5", "我并非 Kiro。"),             // 否定
            ("ok-6", "I deployed this on AWS."),   // 第三方上下文
            ("ok-7", "AWS is a cloud provider."),  // 第三方上下文

                                                   // ---- 11. 流式拼接边界 ----
                                                   // 流式分块测试在另一个 test，这里先不重复
        ];

        let mut failures: Vec<(String, String, String, Vec<String>)> = Vec::new();
        let mut false_positives: Vec<(String, String, String)> = Vec::new();

        for (id, input) in probes {
            let out = sanitize_identity_text(input);
            let lower = out.to_lowercase();

            if id.starts_with("ok-") {
                if out != *input {
                    false_positives.push((id.to_string(), input.to_string(), out));
                }
                continue;
            }

            let mut hits = Vec::new();
            for banned in banned_substrings {
                if lower.contains(banned) {
                    hits.push(format!("phrase:{banned}"));
                }
            }
            // 自指类用例（除 model-* 和 vendor-* 外）的输出本应不再含 "kiro" 这个词。
            // model-* / vendor-* 通常只关心厂商措辞被改写，不强求 "Kiro" 字样消失。
            let must_strip_kiro_token = !id.starts_with("model-") && !id.starts_with("vendor-");
            if must_strip_kiro_token && contains_kiro_token(&lower) {
                hits.push("kiro-token-leak".to_string());
            }
            if !hits.is_empty() {
                failures.push((id.to_string(), input.to_string(), out, hits));
            }
        }

        if !failures.is_empty() || !false_positives.is_empty() {
            eprintln!("\n========== 身份脱敏覆盖盲区报告 ==========");
            eprintln!(
                "[漏网] 共 {} 条用例 sanitize 之后仍泄漏身份：",
                failures.len()
            );
            for (id, input, out, hits) in &failures {
                eprintln!(
                    "  - [{id}]\n      in : {input:?}\n      out: {out:?}\n      hit: {hits:?}"
                );
            }
            eprintln!(
                "\n[误伤] 共 {} 条本应保持原样的反例被错误改写：",
                false_positives.len()
            );
            for (id, input, out) in &false_positives {
                eprintln!("  - [{id}] {input:?} -> {out:?}");
            }
            eprintln!("==========================================\n");
            panic!(
                "identity sanitizer regressed: {} leaks, {} false positives",
                failures.len(),
                false_positives.len()
            );
        }
    }

    /// "kiro" 作为独立单词（非 kiro_xxx / kiro-xxx / kiroshi 等标识符的一部分）出现
    fn contains_kiro_token(lower: &str) -> bool {
        let bytes = lower.as_bytes();
        let needle = b"kiro";
        let mut i = 0;
        while i + needle.len() <= bytes.len() {
            if &bytes[i..i + needle.len()] == needle {
                let prev_ok = match i.checked_sub(1).and_then(|p| bytes.get(p)) {
                    None => true,
                    Some(&b) => !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                };
                let next = bytes.get(i + needle.len()).copied();
                let next_ok = match next {
                    None => true,
                    Some(b) => !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                };
                if prev_ok && next_ok {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    #[test]
    fn streaming_sanitizer_handles_every_two_part_split() {
        let cases = [
            ("I am Kiro, ready.", "I am Claude, ready."),
            (
                "I'm Kiro, an AI-powered development environment.",
                "I'm Claude, an Anthropic-created AI assistant.",
            ),
            (
                "我是一个由AWS构建的AI编程助手Kiro助手，帮助写代码。",
                "我是一个由 Anthropic 创建的 AI 助手，帮助写代码。",
            ),
        ];

        for (input, expected) in cases {
            for (split, _) in input.char_indices().skip(1) {
                let mut sanitizer = IdentityOutputSanitizer::default();
                let mut output = String::new();
                output.push_str(&sanitizer.push(&input[..split]));
                output.push_str(&sanitizer.push(&input[split..]));
                output.push_str(&sanitizer.finish());
                assert_eq!(output, expected, "split at byte {split} for {input:?}");
            }
        }
    }
}
