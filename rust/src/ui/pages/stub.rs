//! Navigation page for surfaces not ported yet. Keeps the push, back
//! button and Escape flow real while the content is a status page.

pub fn page(title: &str, description: &str) -> adw::NavigationPage {
    let status = adw::StatusPage::builder().icon_name("view-list-symbolic").title(title).description(description).build();
    let view = adw::ToolbarView::new();
    view.set_content(Some(&status));
    adw::NavigationPage::builder().child(&view).title(title).build()
}
