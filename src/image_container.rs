use cosmic::iced::widget::{
    image::{draw, FilterMethod, Handle},
    Container,
};
use cosmic::iced::ContentFit;
use cosmic::iced_core::event::{self, Event};
use cosmic::iced_core::layout;
use cosmic::iced_core::mouse;
use cosmic::iced_core::overlay;
use cosmic::iced_core::renderer;
use cosmic::iced_core::widget::{Operation, Tree};
use cosmic::iced_core::{Clipboard, Element, Layout, Length, Rectangle, Shell, Widget};
use cosmic::iced_renderer::core::widget::OperationOutputWrapper;

pub use cosmic::iced_style::container::StyleSheet;

pub struct ImageContainer<'a, Message, Renderer>
where
    Renderer: cosmic::iced_core::Renderer + cosmic::iced_core::image::Renderer<Handle = Handle>,
    Renderer::Theme: StyleSheet,
{
    container: Container<'a, Message, Renderer>,
    image_opt: Option<Handle>,
    content_fit: ContentFit,
}

impl<'a, Message, Renderer> ImageContainer<'a, Message, Renderer>
where
    Renderer: cosmic::iced_core::Renderer + cosmic::iced_core::image::Renderer<Handle = Handle>,
    Renderer::Theme: StyleSheet,
{
    pub fn new(container: Container<'a, Message, Renderer>) -> Self {
        Self {
            container,
            image_opt: None,
            content_fit: ContentFit::None,
        }
    }

    pub fn image(mut self, image: Handle) -> Self {
        self.image_opt = Some(image);
        self
    }

    pub fn content_fit(mut self, content_fit: ContentFit) -> Self {
        self.content_fit = content_fit;
        self
    }
}

impl<'a, Message, Renderer> Widget<Message, Renderer> for ImageContainer<'a, Message, Renderer>
where
    Renderer: cosmic::iced_core::Renderer + cosmic::iced_core::image::Renderer<Handle = Handle>,
    Renderer::Theme: StyleSheet,
{
    fn children(&self) -> Vec<Tree> {
        self.container.children()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.container.diff(tree)
    }

    fn width(&self) -> Length {
        Widget::width(&self.container)
    }

    fn height(&self) -> Length {
        Widget::height(&self.container)
    }

    fn layout(
        &self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.container.layout(tree, renderer, limits)
    }

    fn operate(
        &self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation<OperationOutputWrapper<Message>>,
    ) {
        self.container.operate(tree, layout, renderer, operation)
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) -> event::Status {
        self.container.on_event(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        )
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.container
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Renderer::Theme,
        renderer_style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        match &self.image_opt {
            Some(image) => draw(
                renderer,
                layout,
                image,
                self.content_fit,
                FilterMethod::Linear,
                [0.0, 0.0, 0.0, 0.0],
            ),
            None => {}
        }

        self.container.draw(
            tree,
            renderer,
            theme,
            renderer_style,
            layout,
            cursor,
            viewport,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'b, Message, Renderer>> {
        self.container.overlay(tree, layout, renderer)
    }
}

impl<'a, Message, Renderer> From<ImageContainer<'a, Message, Renderer>>
    for Element<'a, Message, Renderer>
where
    Message: 'a,
    Renderer:
        'a + cosmic::iced_core::Renderer + cosmic::iced_core::image::Renderer<Handle = Handle>,
    Renderer::Theme: StyleSheet,
{
    fn from(container: ImageContainer<'a, Message, Renderer>) -> Element<'a, Message, Renderer> {
        Element::new(container)
    }
}
