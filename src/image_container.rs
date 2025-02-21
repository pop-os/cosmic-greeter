use cosmic::iced::ContentFit;
use cosmic::iced::{
    widget::{
        image::{draw, FilterMethod, Handle},
        Container,
    },
    Rotation,
};
use cosmic::iced_core::event::{self, Event};
use cosmic::iced_core::layout;
use cosmic::iced_core::mouse;
use cosmic::iced_core::overlay;
use cosmic::iced_core::renderer;
use cosmic::iced_core::widget::{Operation, Tree};
use cosmic::iced_core::{Clipboard, Element, Layout, Length, Rectangle, Shell, Size, Widget};
use cosmic::{Renderer, Theme};

pub struct ImageContainer<'a, Message> {
    container: Container<'a, Message, Theme, Renderer>,
    image_opt: Option<Handle>,
    content_fit: ContentFit,
}

impl<'a, Message> ImageContainer<'a, Message> {
    pub fn new(container: Container<'a, Message, Theme, Renderer>) -> Self {
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

impl<'a, Message> Widget<Message, Theme, Renderer> for ImageContainer<'a, Message> {
    fn children(&self) -> Vec<Tree> {
        self.container.children()
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.container.diff(tree)
    }

    fn size(&self) -> Size<Length> {
        self.container.size()
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
        operation: &mut dyn Operation<()>,
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
        theme: &Theme,
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
                Rotation::default(),
                1.,
                [0.0, 0.0, 0.0, 0.0],
            ),
            None => {}
        }

        use cosmic::iced_renderer::core::Renderer as IcedRenderer;
        renderer.with_layer(layout.bounds(), |renderer| {
            self.container.draw(
                tree,
                renderer,
                theme,
                renderer_style,
                layout,
                cursor,
                viewport,
            )
        });
    }

    fn overlay<'b>(
        &'b mut self,
        state: &'b mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        translation: cosmic::iced::Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.container.overlay(state, layout, renderer, translation)
    }
}

impl<'a, Message> From<ImageContainer<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
{
    fn from(container: ImageContainer<'a, Message>) -> Element<'a, Message, Theme, Renderer> {
        Element::new(container)
    }
}
